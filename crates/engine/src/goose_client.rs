//! Goose headless client for AI-assisted fix generation.
//!
//! Shells out to `goose run` with the developer extension to apply
//! complex migration fixes that can't be handled by pattern matching.

use anyhow::{Context, Result};
use fix_engine_core::LlmFixRequest;

use crate::context::FixContext;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Result of a goose fix attempt.
#[derive(Debug)]
pub struct GooseFixResult {
    pub file_path: PathBuf,
    pub rule_id: String,
    pub success: bool,
    pub output: String,
}

/// Run a goose command with a timeout. Returns the combined stdout+stderr
/// output, or an error if the process times out or fails to start.
/// Return type from goose: (success, text_response, raw_json_output)
/// The raw_json_output contains the full goose session including all
/// tool calls, which is invaluable for debugging empty responses.
fn run_goose_with_timeout(
    prompt: &str,
    max_turns: &str,
    timeout_secs: u64,
) -> Result<(bool, String, String)> {
    let mut cmd = Command::new("goose");
    cmd.args([
        "run",
        "--quiet",
        "--text",
        prompt,
        "--with-builtin",
        "developer",
        "--no-session",
        "--max-turns",
        max_turns,
        "--output-format",
        "json",
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .stdin(std::process::Stdio::null());

    // Isolate goose in its own process group so that signals sent by
    // goose's child processes (e.g., claude-code) cannot propagate to
    // our parent process.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .context("Failed to execute goose. Is it installed and in PATH?")?;

    let timeout = Duration::from_secs(timeout_secs);
    let start = std::time::Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Process exited — stdout is JSON with full message history
                let raw_json = child
                    .stdout
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).ok();
                        buf
                    })
                    .unwrap_or_default();
                let _stderr = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).ok();
                        buf
                    })
                    .unwrap_or_default();

                // Extract the text response from the JSON output.
                // The JSON has { "messages": [ { "role": "assistant", "content": [...] } ] }
                // We want the last assistant message's text content.
                let text_response = extract_text_from_goose_json(&raw_json);

                return Ok((status.success(), text_response, raw_json));
            }
            Ok(None) => {
                // Still running — check timeout
                if start.elapsed() >= timeout {
                    // Kill the entire process group (goose + any children like
                    // claude-code) to prevent orphaned processes.
                    #[cfg(unix)]
                    {
                        let pid = child.id() as i32;
                        // SIGTERM first to allow graceful shutdown
                        unsafe {
                            libc::kill(-pid, libc::SIGTERM);
                        }
                        std::thread::sleep(Duration::from_millis(1000));
                        // SIGKILL to ensure cleanup
                        unsafe {
                            libc::kill(-pid, libc::SIGKILL);
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = child.kill();
                    }
                    let _ = child.wait();
                    anyhow::bail!("goose timed out after {}s", timeout_secs);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                anyhow::bail!("Failed to wait on goose process: {}", e);
            }
        }
    }
}

/// Run goose fixes for all pending LLM requests.
/// Groups requests by file path for batch processing.
/// If `log_dir` is provided, saves prompts and responses to JSON files.
/// An LLM fix request with multiple incidents from the same rule merged
/// into a single entry. This preserves the priority-based sort order
/// (hierarchy rules first) rather than re-sorting by rule_id.
#[derive(Debug)]
struct MergedLlmFixRequest {
    rule_id: String,
    file_path: PathBuf,
    /// All incident line numbers (may be a single line).
    lines: Vec<u32>,
    /// Rule message (shared across all incidents of the same rule).
    message: String,
    /// Code snippets keyed by line number.
    code_snips: Vec<(u32, String)>,
    /// Component family (e.g., "Modal", "Select") extracted from labels.
    /// Used to group related rules in the batch prompt.
    family: Option<String>,
}

/// Merge LLM fix requests by rule_id, preserving insertion order.
///
/// Multiple incidents from the same rule (e.g., a composition rule firing
/// at different lines) are collapsed into a single entry with all affected
/// lines and code snippets combined.
fn merge_by_rule_id(requests: &[&LlmFixRequest]) -> Vec<MergedLlmFixRequest> {
    let mut merged: Vec<MergedLlmFixRequest> = Vec::new();
    let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();

    for req in requests {
        if let Some(&idx) = index.get(req.rule_id.as_str()) {
            merged[idx].lines.push(req.line);
            if let Some(snip) = &req.code_snip {
                merged[idx].code_snips.push((req.line, snip.clone()));
            }
        } else {
            let idx = merged.len();
            index.insert(&req.rule_id, idx);
            let code_snips = req
                .code_snip
                .as_ref()
                .map(|s| vec![(req.line, s.clone())])
                .unwrap_or_default();
            let family = req
                .labels
                .iter()
                .find(|l| l.starts_with("family="))
                .and_then(|l| l.strip_prefix("family="))
                .map(|s| s.to_string());
            merged.push(MergedLlmFixRequest {
                rule_id: req.rule_id.clone(),
                file_path: req.file_path.clone(),
                lines: vec![req.line],
                message: req.message.clone(),
                code_snips,
                family,
            });
        }
    }

    merged
}

/// Extract the text response from goose's JSON output format.
///
/// Goose's `--output-format json` returns:
/// ```json
/// { "messages": [ { "role": "assistant", "content": [{ "type": "text", "text": "..." }] } ] }
/// ```
/// We extract the text from the LAST assistant message.
fn extract_text_from_goose_json(raw_json: &str) -> String {
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(raw_json);
    match parsed {
        Ok(json) => {
            if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
                // Find the last assistant message
                for msg in messages.iter().rev() {
                    if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                        if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                            // Collect all text blocks
                            let texts: Vec<&str> = content
                                .iter()
                                .filter_map(|c| {
                                    if c.get("type").and_then(|t| t.as_str()) == Some("text") {
                                        c.get("text").and_then(|t| t.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            if !texts.is_empty() {
                                return texts.join("\n");
                            }
                        }
                    }
                }
                // Valid JSON with messages array but no assistant text —
                // goose ran but produced no output. Return empty so the
                // retry logic can detect this and retry.
                return String::new();
            }
            // No messages array at all — not goose JSON format.
            // Return as-is (might be plain text from older goose).
            raw_json.to_string()
        }
        Err(_) => {
            // Not valid JSON — return as-is (might be plain text from older goose)
            raw_json.to_string()
        }
    }
}

/// Extract the "## Changes Applied" section from the LLM's response.
///
/// The prompt instructs the LLM to produce this section after writing the file.
/// We extract it verbatim to pass as continuation context to the next chunk,
/// so subsequent chunks know what was actually changed (not just what was requested).
///
/// Falls back to "## Summary of Changes", "## Summary of changes", or
/// "## Changes Applied" variants for robustness.
fn extract_changes_applied(response: &str) -> Option<String> {
    // Try multiple header patterns the LLM might use
    let markers = [
        "## Changes Applied",
        "## Summary of Changes",
        "## Summary of changes",
        "## Summary",
    ];

    for marker in &markers {
        if let Some(start) = response.find(marker) {
            let section = &response[start..];
            // Trim to just this section — stop at the next top-level heading
            // or end of response.
            let end = section[marker.len()..]
                .find("\n## ")
                .map(|pos| marker.len() + pos)
                .unwrap_or(section.len());
            let trimmed = section[..end].trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    None
}

/// Maximum number of files to process concurrently.
/// Each file spawns a goose process, so this limits system load.
const MAX_CONCURRENT_FILES: usize = 3;

pub fn run_all_goose_fixes(
    requests: &[LlmFixRequest],
    ctx: &dyn FixContext,
    verbose: bool,
    log_dir: Option<&std::path::Path>,
    printer: &crate::progress::ProgressPrinter,
    timeout_secs: u64,
    max_families_per_chunk: usize,
) -> Vec<GooseFixResult> {
    // Create log directory if specified
    if let Some(dir) = log_dir {
        let _ = std::fs::create_dir_all(dir);
    }

    // Group by file path for batching
    let mut by_file: BTreeMap<PathBuf, Vec<&LlmFixRequest>> = BTreeMap::new();
    for req in requests {
        by_file.entry(req.file_path.clone()).or_default().push(req);
    }

    // Merge incidents from the same rule within each file, then sort by
    // priority so the most impactful structural migration rules (hierarchy
    // composition) come first in each batch. This ensures:
    //  1. Multiple incidents from the same rule are presented as one fix.
    //  2. The first chunk starts with structural migration rules that
    //     trigger tool calls (file reads/edits), preventing empty goose output.
    //  3. Informational/review-only rules come last where they're less likely
    //     to consume turns or confuse the LLM.
    let mut merged_by_file: Vec<(PathBuf, Vec<MergedLlmFixRequest>)> = Vec::new();
    for (path, file_reqs) in by_file {
        let mut merged = merge_by_rule_id(&file_reqs);
        // Sort family rules first (they represent coherent migrations that
        // should be processed as early as possible), grouped by family name
        // so same-family rules are adjacent for chunking. Within each group,
        // sort by individual priority. Non-family rules come after.
        merged.sort_by(|a, b| {
            let a_has_family = a.family.is_some();
            let b_has_family = b.family.is_some();
            match (a_has_family, b_has_family) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (true, true) => a.family.cmp(&b.family).then_with(|| {
                    ctx.fix_priority(&a.rule_id)
                        .cmp(&ctx.fix_priority(&b.rule_id))
                }),
                (false, false) => ctx
                    .fix_priority(&a.rule_id)
                    .cmp(&ctx.fix_priority(&b.rule_id)),
            }
        });
        merged_by_file.push((path, merged));
    }

    let total_files = merged_by_file.len();
    let total_fixes = requests.len();

    let bar = printer.start_counted(
        &format!(
            "Goose fixes ({} fixes across {} files, {} concurrent)",
            total_fixes, total_files, MAX_CONCURRENT_FILES,
        ),
        total_files as u64,
    );

    let pipeline_start = std::time::Instant::now();
    let succeeded = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let failed_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Use a channel as a simple concurrency limiter (semaphore)
    let (sem_tx, sem_rx) = std::sync::mpsc::sync_channel::<()>(MAX_CONCURRENT_FILES);
    for _ in 0..MAX_CONCURRENT_FILES {
        sem_tx.send(()).unwrap();
    }

    let file_entries: Vec<(usize, PathBuf, Vec<MergedLlmFixRequest>)> = merged_by_file
        .into_iter()
        .enumerate()
        .map(|(i, (path, reqs))| (i, path, reqs))
        .collect();

    // Mutex to serialize flushing of per-file log buffers so that
    // output for each file appears as a contiguous block even when
    // multiple files finish at roughly the same time.
    let flush_lock = std::sync::Mutex::new(());

    let results: Vec<GooseFixResult> = std::thread::scope(|s| {
        let mut handles = Vec::new();

        for (i, file_path, file_requests) in &file_entries {
            // Acquire semaphore slot (blocks until a slot is free)
            sem_rx.recv().unwrap();

            let sem_tx = sem_tx.clone();
            let ok_count = succeeded.clone();
            let fail_count = failed_count.clone();
            let i = *i;
            let printer = printer.clone();
            let bar = bar.clone();
            let flush_lock = &flush_lock;

            let handle = s.spawn(move || {
                let (result, log_lines) = process_single_file(
                    i,
                    total_files,
                    file_path,
                    file_requests,
                    ctx,
                    verbose,
                    log_dir,
                    timeout_secs,
                    max_families_per_chunk,
                );

                match &result {
                    r if r.success => {
                        ok_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    _ => {
                        fail_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                bar.inc();

                // Flush all log lines as a contiguous block under the
                // mutex so output from different files never interleaves.
                {
                    let _guard = flush_lock.lock().unwrap();
                    for line in &log_lines {
                        printer.println(line);
                    }
                }

                // Release semaphore slot
                let _ = sem_tx.send(());
                result
            });

            handles.push(handle);
        }

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    bar.finish();

    let total_elapsed = pipeline_start.elapsed();
    let ok = succeeded.load(std::sync::atomic::Ordering::Relaxed);
    let fail = failed_count.load(std::sync::atomic::Ordering::Relaxed);
    printer.println(&format!(
        "  Goose complete: {} succeeded, {} failed ({:.0}s total, {:.1}s avg per file)",
        ok,
        fail,
        total_elapsed.as_secs_f64(),
        total_elapsed.as_secs_f64() / total_files.max(1) as f64,
    ));

    results
}

/// Process all fixes for a single file. Chunks are processed sequentially
/// within the file (each chunk reads the file as modified by the previous).
/// This function is called from parallel threads — one per file.
///
/// Returns `(GooseFixResult, Vec<String>)` where the second element is
/// buffered log lines. The caller flushes these through the shared printer
/// under a mutex so that per-file output is never interleaved.
fn process_single_file(
    file_index: usize,
    total_files: usize,
    file_path: &std::path::Path,
    file_requests: &[MergedLlmFixRequest],
    ctx: &dyn FixContext,
    verbose: bool,
    log_dir: Option<&std::path::Path>,
    timeout_secs: u64,
    max_families_per_chunk: usize,
) -> (GooseFixResult, Vec<String>) {
    let mut log: Vec<String> = Vec::new();

    let file_name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| file_path.display().to_string());

    let rule_ids: Vec<&str> = file_requests.iter().map(|r| r.rule_id.as_str()).collect();
    let rules_display = if rule_ids.len() <= 3 {
        rule_ids.join(", ")
    } else {
        format!(
            "{}, ... +{} more",
            rule_ids[..2].join(", "),
            rule_ids.len() - 2
        )
    };

    log.push(format!(
        "  [{}/{}] {} ({} fixes) [{}]",
        file_index + 1,
        total_files,
        file_name,
        file_requests.len(),
        rules_display,
    ));

    let file_start = std::time::Instant::now();

    // Split large batches into chunks to avoid overwhelming the LLM.
    // Each chunk runs sequentially — the LLM reads the file as modified
    // by the previous chunk. A context summary of previously applied
    // fixes is prepended to each subsequent chunk.
    let mut result: Result<GooseFixResult> = Ok(GooseFixResult {
        file_path: file_path.to_path_buf(),
        rule_id: String::new(),
        success: true,
        output: String::new(),
    });
    let mut all_prompts = Vec::new();
    let mut all_outputs: Vec<String> = Vec::new();
    let mut all_stderrs: Vec<String> = Vec::new();
    let mut chunk_times: Vec<f64> = Vec::new();
    let mut chunk_retried: Vec<bool> = Vec::new();
    let mut applied_summaries: Vec<String> = Vec::new();

    if file_requests.len() == 1 {
        let prompt = build_merged_prompt(&file_requests[0], ctx);
        all_prompts.push(prompt.clone());
        let max_turns_str = "5".to_string();
        let mut goose_result = run_goose_with_timeout(&prompt, &max_turns_str, timeout_secs);
        let mut was_retried = false;
        // Retry once on empty response
        if let Ok((_, ref output, _)) = goose_result {
            if output.len() <= 1 {
                was_retried = true;
                log.push(format!("         {}: empty response — retrying once...", file_name));
                std::thread::sleep(Duration::from_secs(2));
                goose_result = run_goose_with_timeout(&prompt, &max_turns_str, timeout_secs);
            }
        }
        match goose_result {
            Ok((success, output, stderr)) => {
                all_outputs.push(output.clone());
                all_stderrs.push(stderr);
                chunk_times.push(0.0);
                chunk_retried.push(was_retried);
                result = Ok(GooseFixResult {
                    file_path: file_path.to_path_buf(),
                    rule_id: file_requests[0].rule_id.clone(),
                    success,
                    output,
                });
            }
            Err(e) => {
                result = Err(e);
            }
        }
    } else {
        // Build family-aware chunks. Rules from the same component family
        // must stay in the same chunk so the LLM sees the full migration
        // (composition + prop→child + conformance) as one coherent change.
        // A family group is treated as one logical unit regardless of size.
        //
        // Chunking is driven by the number of *distinct* component families
        // in the current chunk (`max_families_per_chunk`). Family rules are
        // the complex structural migrations that dominate LLM processing
        // time, so limiting them keeps each goose invocation focused.
        // Non-family rules (simple renames, import swaps) do not count
        // against the family limit — they're cheap and fill remaining space.
        // A secondary guard (`max_fixes_per_batch`) caps the total number
        // of rules per chunk to avoid overwhelming the LLM regardless.
        let max_fixes_per_batch = 8;
        let chunks: Vec<Vec<&MergedLlmFixRequest>> = {
            let mut result: Vec<Vec<&MergedLlmFixRequest>> = Vec::new();
            let mut current_chunk: Vec<&MergedLlmFixRequest> = Vec::new();
            let mut current_families: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for req in file_requests.iter() {
                if let Some(ref fam) = req.family {
                    if current_families.contains(fam) {
                        // Same family — always add to current chunk
                        current_chunk.push(req);
                    } else if current_families.len() >= max_families_per_chunk
                        && !current_chunk.is_empty()
                    {
                        // New family but we've hit the family limit — start new chunk
                        result.push(std::mem::take(&mut current_chunk));
                        current_families.clear();
                        current_families.insert(fam.clone());
                        current_chunk.push(req);
                    } else if current_chunk.len() >= max_fixes_per_batch
                        && !current_chunk.is_empty()
                    {
                        // New family and chunk is full — start new chunk
                        result.push(std::mem::take(&mut current_chunk));
                        current_families.clear();
                        current_families.insert(fam.clone());
                        current_chunk.push(req);
                    } else {
                        // New family, chunk has room
                        current_families.insert(fam.clone());
                        current_chunk.push(req);
                    }
                } else {
                    // No family — add to current chunk, respect size limit
                    if current_chunk.len() >= max_fixes_per_batch {
                        result.push(std::mem::take(&mut current_chunk));
                        current_families.clear();
                    }
                    current_chunk.push(req);
                }
            }
            if !current_chunk.is_empty() {
                result.push(current_chunk);
            }
            result
        };
        let chunk_count = chunks.len();

        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            if chunk_idx > 0 {
                log.push(format!(
                    "         {}: chunk {}/{} ({} fixes)...",
                    file_name,
                    chunk_idx + 1,
                    chunk_count,
                    chunk.len()
                ));
            }

            let chunk_refs: Vec<&MergedLlmFixRequest> = chunk.to_vec();

            let prompt = build_batch_prompt_with_context(
                file_path,
                &chunk_refs,
                if applied_summaries.is_empty() {
                    None
                } else {
                    Some(&applied_summaries)
                },
                ctx,
            );
            all_prompts.push(prompt.clone());

            let max_turns = (22 + chunk.len()).min(40);
            let max_turns_str = max_turns.to_string();
            let chunk_start = std::time::Instant::now();
            let mut chunk_result = run_goose_with_timeout(&prompt, &max_turns_str, timeout_secs);
            let mut was_retried = false;

            // Retry once on empty response. Goose sometimes returns an
            // empty assistant message (no tool calls, no text) due to
            // transient LLM API issues or serialization failures.
            // NOTE: The first attempt may have made PARTIAL edits to the
            // file before failing to produce a summary. The retry prompt
            // accounts for this.
            if let Ok((_, ref output, _)) = chunk_result {
                if output.len() <= 1 {
                    was_retried = true;
                    log.push(format!(
                        "         {}: chunk {}/{} empty response — retrying once...",
                        file_name,
                        chunk_idx + 1,
                        chunk_count,
                    ));
                    std::thread::sleep(Duration::from_secs(2));
                    let retry_prompt = format!(
                        "{}\n\n\
                         RETRY: The previous attempt may have made PARTIAL changes to the file but did not complete. \
                         You MUST read the file as it exists NOW on disk, check each fix individually, \
                         and apply every fix that is not yet present. Do not assume all fixes are applied \
                         just because some are — check EVERY one.",
                        prompt,
                    );
                    chunk_result = run_goose_with_timeout(&retry_prompt, &max_turns_str, timeout_secs);
                }
            }

            let chunk_elapsed = chunk_start.elapsed();

            match chunk_result {
                Ok((success, output, stderr)) => {
                    let resp_len = output.len();
                    let status = if resp_len <= 1 {
                        "EMPTY"
                    } else if success {
                        "ok"
                    } else {
                        "FAILED"
                    };
                    // Count messages in the goose JSON to understand what happened
                    let msg_count = serde_json::from_str::<serde_json::Value>(&stderr)
                        .ok()
                        .and_then(|j| j.get("messages")?.as_array().map(|a| a.len()))
                        .unwrap_or(0);

                    log.push(format!(
                        "         {}: chunk {}/{} {} ({} fixes, {:.1}s, response={} chars, goose_messages={})",
                        file_name,
                        chunk_idx + 1,
                        chunk_count,
                        status,
                        chunk.len(),
                        chunk_elapsed.as_secs_f64(),
                        resp_len,
                        msg_count,
                    ));
                    // If empty response, summarize what goose did from the JSON
                    if resp_len <= 1 {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stderr) {
                            if let Some(messages) = json.get("messages").and_then(|m| m.as_array())
                            {
                                for msg in messages {
                                    let role =
                                        msg.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                                    if role == "assistant" {
                                        if let Some(content) =
                                            msg.get("content").and_then(|c| c.as_array())
                                        {
                                            for item in content {
                                                let typ = item
                                                    .get("type")
                                                    .and_then(|t| t.as_str())
                                                    .unwrap_or("?");
                                                if typ == "toolUse" {
                                                    let tool = item
                                                        .get("name")
                                                        .and_then(|n| n.as_str())
                                                        .unwrap_or("?");
                                                    log.push(format!(
                                                        "           goose: tool_call={}",
                                                        tool
                                                    ));
                                                } else if typ == "text" {
                                                    let text = item
                                                        .get("text")
                                                        .and_then(|t| t.as_str())
                                                        .unwrap_or("");
                                                    if !text.is_empty() {
                                                        log.push(format!(
                                                            "           goose: text={}",
                                                            &text[..text.len().min(100)]
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        } else if msg_count == 0 {
                            log.push("           goose: no messages in output (goose may have failed silently)".to_string());
                        }
                    }

                    chunk_times.push(chunk_elapsed.as_secs_f64());
                    chunk_retried.push(was_retried);
                    all_stderrs.push(stderr);

                    // Record what was applied for context in next chunk.
                    // Extract the "## Changes Applied" section from the LLM's
                    // response, which describes what was actually done (or not).
                    // This is more useful than echoing the fix descriptions,
                    // because it tells the next chunk what the file looks like
                    // now, not what was requested.
                    if let Some(summary) = extract_changes_applied(&output) {
                        applied_summaries.push(summary);
                    } else {
                        // Fallback: use first line of each fix description
                        for req in chunk.iter() {
                            let lines_display = req
                                .lines
                                .iter()
                                .map(|l| l.to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            let first_line =
                                req.message.lines().next().unwrap_or("(no description)");
                            applied_summaries.push(format!(
                                "- {} (line {}): {}",
                                req.rule_id, lines_display, first_line
                            ));
                        }
                    }
                    all_outputs.push(output.clone());
                    result = Ok(GooseFixResult {
                        file_path: file_path.to_path_buf(),
                        rule_id: file_requests
                            .iter()
                            .map(|r| r.rule_id.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        success,
                        output,
                    });
                    if !success {
                        log.push(format!(
                            "         {}: chunk {}/{} FAILED, stopping",
                            file_name,
                            chunk_idx + 1,
                            chunk_count
                        ));
                        break;
                    }
                }
                Err(e) => {
                    // Retry on timeout
                    let err_msg = format!("{}", e);
                    if err_msg.contains("timed out") {
                        let backoff = Duration::from_secs(10);
                        log.push(format!(
                            "         {}: chunk {}/{} timed out after {:.1}s, retrying in {}s...",
                            file_name,
                            chunk_idx + 1,
                            chunk_count,
                            chunk_elapsed.as_secs_f64(),
                            backoff.as_secs(),
                        ));
                        std::thread::sleep(backoff);
                        let _retry_start = std::time::Instant::now();
                        let retry_result = run_goose_with_timeout(&prompt, &max_turns_str, timeout_secs);
                        match retry_result {
                            Ok((success, output, _retry_stderr)) => {
                                for req in chunk.iter() {
                                    let lines_display = req
                                        .lines
                                        .iter()
                                        .map(|l| l.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    let summary: String = req
                                        .message
                                        .lines()
                                        .take(3)
                                        .collect::<Vec<_>>()
                                        .join("\n  ");
                                    applied_summaries.push(format!(
                                        "- {} (line {}): {}",
                                        req.rule_id, lines_display, summary
                                    ));
                                }
                                all_outputs.push(output.clone());
                                result = Ok(GooseFixResult {
                                    file_path: file_path.to_path_buf(),
                                    rule_id: file_requests
                                        .iter()
                                        .map(|r| r.rule_id.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    success,
                                    output,
                                });
                            }
                            Err(e2) => {
                                result = Err(e2);
                                break;
                            }
                        }
                    } else {
                        result = Err(e);
                        break;
                    }
                }
            }
        }
    }

    let elapsed = file_start.elapsed();

    match result {
        Ok(r) => {
            if r.success {
                log.push(format!("         {}: ok ({:.1}s)", file_name, elapsed.as_secs_f64()));
            } else {
                log.push(format!(
                    "         {}: FAILED ({:.1}s)",
                    file_name,
                    elapsed.as_secs_f64()
                ));
            }
            if verbose && !r.output.is_empty() {
                // Show the structured "Changes Applied" section if the LLM
                // produced one; otherwise show the full raw output.
                if let Some(summary) = extract_changes_applied(&r.output) {
                    for line in summary.lines() {
                        log.push(format!("           {}", line));
                    }
                } else {
                    for line in r.output.lines() {
                        log.push(format!("           {}", line));
                    }
                }
            }

            // Save all prompts + responses to log file (one entry per chunk)
            if let Some(dir) = log_dir {
                let chunks: Vec<serde_json::Value> = all_prompts
                    .iter()
                    .enumerate()
                    .map(|(i, prompt)| {
                        let resp = all_outputs.get(i).unwrap_or(&String::new()).clone();
                        let raw_json = all_stderrs.get(i).unwrap_or(&String::new()).clone();
                        let resp_len = resp.len();
                        // Parse the raw goose JSON for structured logging
                        let goose_session: serde_json::Value = serde_json::from_str(&raw_json)
                            .unwrap_or_else(|_| serde_json::json!({"raw": raw_json}));
                        serde_json::json!({
                            "chunk": i + 1,
                            "prompt": prompt,
                            "response": resp,
                            "response_length": resp_len,
                            "elapsed_secs": chunk_times.get(i).unwrap_or(&0.0),
                            "retried": chunk_retried.get(i).unwrap_or(&false),
                            "status": if resp_len <= 1 { "empty" } else { "ok" },
                            "goose_session": goose_session,
                        })
                    })
                    .collect();

                let log_entry = serde_json::json!({
                    "file": file_path.display().to_string(),
                    "rule_ids": file_requests.iter().map(|r| &r.rule_id).collect::<Vec<_>>(),
                    "chunks": chunks,
                    "total_chunks": all_prompts.len(),
                    "success": r.success,
                    "elapsed_secs": elapsed.as_secs_f64(),
                });
                let log_file = dir.join(format!("goose-fix-{:03}.json", file_index + 1));
                let _ = std::fs::write(
                    &log_file,
                    serde_json::to_string_pretty(&log_entry).unwrap_or_default(),
                );
            }

            (r, log)
        }
        Err(e) => {
            log.push(format!(
                "         {}: ERROR ({:.1}s) — {}",
                file_name,
                elapsed.as_secs_f64(),
                e
            ));
            (GooseFixResult {
                file_path: file_path.to_path_buf(),
                rule_id: file_requests
                    .iter()
                    .map(|r| r.rule_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                success: false,
                output: format!("Error: {}", e),
            }, log)
        }
    }
}

// ── Prompt construction ───────────────────────────────────────────────────

/// Build a prompt for a single merged fix request (one unique rule, possibly
/// multiple incident lines).
fn build_merged_prompt(request: &MergedLlmFixRequest, ctx: &dyn FixContext) -> String {
    let lines_display = request
        .lines
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let mut code_context = String::new();
    if request.code_snips.is_empty() {
        code_context.push_str("(no code snippet available)");
    } else if request.code_snips.len() == 1 {
        code_context.push_str(&request.code_snips[0].1);
    } else {
        for (line, snip) in &request.code_snips {
            code_context.push_str(&format!("  (line {}):\n{}\n", line, snip));
        }
    }

    let constraints = ctx.llm_constraints();
    let constraints_section = if constraints.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = constraints.iter().map(|c| format!("- {}", c)).collect();
        format!("\nIMPORTANT constraints:\n{}", lines.join("\n"))
    };

    format!(
        r#"You are applying a {migration_desc} fix.

File: {file_path}
Line: {lines}

Migration rule [{rule_id}]:
{message}

Code context:
```
{code_context}
```

Instructions:
1. Read the file at {file_path}
2. Apply ONLY the change described by the migration rule at or near line {lines}
3. Make the minimum edit necessary — do not change unrelated code, but DO clean up any artifacts caused by your change (e.g., remove imports that are no longer referenced, delete dead declarations)
4. After determining your changes, reason through any functional side effects: verify that your edits preserve the existing behavior of the surrounding code. If a migration change makes existing variables, state, or logic redundant, clean up the affected code so the file remains correct and consistent.
5. Write the fixed file
{constraints_section}

Before writing, reason through the fix step by step to ensure nothing is missed. Then read the file, make the edit, and write it.

After writing the file, produce a '## Changes Applied' section that lists the change you made, or note if the fix was already applied or could not be applied (with a brief reason)."#,
        migration_desc = ctx.migration_description(),
        file_path = request.file_path.display(),
        lines = lines_display,
        rule_id = request.rule_id,
        message = request.message,
        code_context = code_context,
        constraints_section = constraints_section,
    )
}

/// Format a single fix entry in the batch prompt.
fn format_fix_entry(fixes: &mut String, fix_num: usize, req: &MergedLlmFixRequest) {
    let lines_display = req
        .lines
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    if req.lines.len() == 1 {
        let code_context = req
            .code_snips
            .first()
            .map(|(_, s)| s.as_str())
            .unwrap_or("(no snippet)");
        fixes.push_str(&format!(
            r#"
### Fix {num}
Line: {line}
Rule [{rule_id}]:
{message}

Code context:
```
{code_context}
```
"#,
            num = fix_num,
            line = lines_display,
            rule_id = req.rule_id,
            message = req.message,
            code_context = code_context,
        ));
    } else {
        let mut all_snippets = String::new();
        for (line, snip) in &req.code_snips {
            all_snippets.push_str(&format!("  (line {}):\n{}\n", line, snip));
        }
        fixes.push_str(&format!(
            r#"
### Fix {num}
Lines: {lines}
Rule [{rule_id}]:
{message}

This rule affects multiple locations in the file. Apply ALL steps together as one logical change.

Code contexts:
```
{all_snippets}```
"#,
            num = fix_num,
            lines = lines_display,
            rule_id = req.rule_id,
            message = req.message,
            all_snippets = all_snippets,
        ));
    }
}

fn build_batch_prompt_with_context(
    file_path: &std::path::Path,
    requests: &[&MergedLlmFixRequest],
    previously_applied: Option<&[String]>,
    ctx: &dyn FixContext,
) -> String {
    // Group requests by component family so the LLM sees related rules
    // as one coherent migration (e.g., all Modal prop→child + composition
    // rules together) rather than independent fixes.
    let mut fixes = String::new();
    let mut fix_num = 0usize;

    // Partition into family-grouped and ungrouped
    let mut family_groups: std::collections::BTreeMap<String, Vec<&MergedLlmFixRequest>> =
        std::collections::BTreeMap::new();
    let mut ungrouped: Vec<&MergedLlmFixRequest> = Vec::new();

    for req in requests.iter() {
        if let Some(ref fam) = req.family {
            family_groups.entry(fam.clone()).or_default().push(req);
        } else {
            ungrouped.push(req);
        }
    }

    // Emit family-grouped fixes first (they're higher priority structurally)
    for (family, group) in &family_groups {
        if group.len() > 1 {
            fixes.push_str(&format!(
                "\n## {} Migration (apply as ONE coherent change)\n\
                 The following {} rules are all part of the {} component family migration.\n\
                 Apply them together — they describe different aspects of the same restructuring.\n",
                family,
                group.len(),
                family,
            ));
        }

        for req in group {
            fix_num += 1;
            format_fix_entry(&mut fixes, fix_num, req);
        }
    }

    // Then emit ungrouped fixes
    for req in &ungrouped {
        fix_num += 1;
        format_fix_entry(&mut fixes, fix_num, req);
    }

    let revert_warning = ctx.revert_warnings().unwrap_or("");
    let context_section = if let Some(applied) = previously_applied {
        let mut section = "\n## Changes from previous pass:\n\
             The following changes were made in a previous pass and are already applied\n\
             to the file on disk. Do NOT revert these changes.\n"
            .to_string();
        if !revert_warning.is_empty() {
            section.push_str(revert_warning);
            section.push('\n');
        }
        section.push_str(
            "If any listed change was NOT actually applied (the old pattern still exists\n\
             in the file), apply it now along with the new fixes below.\n\n",
        );
        section.push_str(&applied.join("\n"));
        section.push_str("\n\n");
        section
    } else {
        String::new()
    };

    let constraints = ctx.llm_constraints();
    let constraints_section = if constraints.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = constraints.iter().map(|c| format!("- {}", c)).collect();
        format!("\nIMPORTANT constraints:\n{}", lines.join("\n"))
    };

    let verification_section = ctx
        .verification_prompt()
        .map(|v| format!("\n{}\n", v))
        .unwrap_or_default();

    format!(
        r#"You are applying {migration_desc} fixes to a single file.

File: {file_path}
{context_section}
Apply ALL of the following {count} fixes to this file:
{fixes}
Instructions:
1. Read the file at {file_path}
2. Process each fix INDEPENDENTLY in sequence. For each fix:
   a. Identify the exact code affected (line number and affected element)
   b. Determine the specific change needed ({change_examples})
   c. Track all changes for the final write
3. Make the minimum edits necessary — do not change unrelated code, but DO clean up any artifacts caused by your changes (e.g., remove imports that are no longer referenced, delete dead declarations)
4. After determining your changes, reason through any functional side effects: verify that your edits preserve the existing behavior of the surrounding code. If a migration change makes existing variables, state, or logic redundant, clean up the affected code so the file remains correct and consistent.
5. Do NOT revert any changes that were already applied in previous passes
6. Write the fixed file once with ALL changes from every fix applied
{constraints_section}
{verification_section}
Before writing, reason through each fix step by step to ensure nothing is missed. Then read the file, make the edits, and write it.

After writing the file, produce a '## Changes Applied' section that lists each change you made, each fix that was already applied (no change needed), and each fix you could not apply (with a brief reason). This summary is used by subsequent processing steps."#,
        migration_desc = ctx.migration_description(),
        file_path = file_path.display(),
        context_section = context_section,
        count = requests.len(),
        fixes = fixes,
        constraints_section = constraints_section,
        change_examples = ctx.change_type_examples(),
        verification_section = verification_section,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_req(rule_id: &str) -> LlmFixRequest {
        make_req_at_line(rule_id, 1, None)
    }

    fn make_req_at_line(rule_id: &str, line: u32, code_snip: Option<&str>) -> LlmFixRequest {
        LlmFixRequest {
            file_path: PathBuf::from("/tmp/test.tsx"),
            file_uri: "file:///tmp/test.tsx".to_string(),
            line,
            rule_id: rule_id.to_string(),
            message: format!("Migration for {}", rule_id),
            code_snip: code_snip.map(|s| s.to_string()),
            source: None,
            labels: Vec::new(),
        }
    }

    // NOTE: fix_priority tests have moved to the patternfly-fix-context crate,
    // since priority ordering is now a FixContext concern, not a goose_client one.

    // ── merge_by_rule_id tests ───────────────────────────────────────────

    #[test]
    fn test_merge_by_rule_id_no_duplicates() {
        let reqs = vec![make_req("rule-a"), make_req("rule-b"), make_req("rule-c")];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();
        let merged = merge_by_rule_id(&refs);

        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].rule_id, "rule-a");
        assert_eq!(merged[1].rule_id, "rule-b");
        assert_eq!(merged[2].rule_id, "rule-c");
        assert_eq!(merged[0].lines, vec![1]);
        assert_eq!(merged[1].lines, vec![1]);
        assert_eq!(merged[2].lines, vec![1]);
    }

    #[test]
    fn test_merge_by_rule_id_combines_same_rule() {
        let reqs = vec![
            make_req_at_line("rule-a", 7, Some("line 7 code")),
            make_req_at_line("rule-b", 10, None),
            make_req_at_line("rule-a", 152, Some("line 152 code")),
        ];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();
        let merged = merge_by_rule_id(&refs);

        assert_eq!(merged.len(), 2);
        // rule-a appears first (first occurrence)
        assert_eq!(merged[0].rule_id, "rule-a");
        assert_eq!(merged[0].lines, vec![7, 152]);
        assert_eq!(merged[0].code_snips.len(), 2);
        assert_eq!(merged[0].code_snips[0], (7, "line 7 code".to_string()));
        assert_eq!(merged[0].code_snips[1], (152, "line 152 code".to_string()));
        // rule-b is second
        assert_eq!(merged[1].rule_id, "rule-b");
        assert_eq!(merged[1].lines, vec![10]);
    }

    #[test]
    fn test_merge_preserves_insertion_order() {
        // Simulate the order after priority sort: hierarchy first, then
        // composition, then prop-level, then conformance.
        let reqs = vec![
            make_req_at_line("semver-hierarchy-modal-composition-changed", 9, None),
            make_req_at_line("semver-hierarchy-emptystate-composition-changed", 6, None),
            make_req_at_line("semver-composition-button-children-to-icon-prop", 139, None),
            make_req_at_line(
                "semver-composition-emptystateheader-nesting-changed",
                152,
                None,
            ),
            make_req_at_line(
                "semver-emptystateheader-component-import-deprecated",
                7,
                None,
            ),
            make_req_at_line(
                "semver-composition-emptystateheader-nesting-changed",
                7,
                None,
            ), // dup at different line
            make_req_at_line("conformance-table-expected-children", 14, None),
        ];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();
        let merged = merge_by_rule_id(&refs);

        // 6 unique rules (emptystateheader-nesting-changed merges two lines)
        assert_eq!(merged.len(), 6);
        // Verify order matches insertion (priority) order
        assert_eq!(
            merged[0].rule_id,
            "semver-hierarchy-modal-composition-changed"
        );
        assert_eq!(
            merged[1].rule_id,
            "semver-hierarchy-emptystate-composition-changed"
        );
        assert_eq!(
            merged[2].rule_id,
            "semver-composition-button-children-to-icon-prop"
        );
        assert_eq!(
            merged[3].rule_id,
            "semver-composition-emptystateheader-nesting-changed"
        );
        assert_eq!(merged[3].lines, vec![152, 7]); // both lines preserved
        assert_eq!(
            merged[4].rule_id,
            "semver-emptystateheader-component-import-deprecated"
        );
        assert_eq!(merged[5].rule_id, "conformance-table-expected-children");
    }

    #[test]
    fn test_merge_then_sort_with_context() {
        // Tests that merge + sort works with a FixContext.
        // With GenericFixContext (priority 3 for all), insertion order is preserved.
        let reqs = vec![
            make_req_at_line("rule-a", 10, None),
            make_req_at_line("rule-b", 20, None),
            make_req_at_line("rule-c", 30, None),
        ];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();

        let mut merged = merge_by_rule_id(&refs);
        let ctx = crate::context::GenericFixContext;
        merged.sort_by(|a, b| {
            ctx.fix_priority(&a.rule_id)
                .cmp(&ctx.fix_priority(&b.rule_id))
        });

        // With equal priority, sort is stable — insertion order preserved
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].rule_id, "rule-a");
        assert_eq!(merged[1].rule_id, "rule-b");
        assert_eq!(merged[2].rule_id, "rule-c");
    }

    #[test]
    fn test_batch_prompt_includes_all_fixes() {
        // Verify that the batch prompt includes all fix entries.
        let reqs = vec![
            make_req("rule-alpha"),
            make_req("rule-beta"),
            make_req("rule-gamma"),
        ];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();

        let merged = merge_by_rule_id(&refs);
        let merged_refs: Vec<&MergedLlmFixRequest> = merged.iter().collect();
        let ctx = crate::context::GenericFixContext;
        let prompt = build_batch_prompt_with_context(
            &PathBuf::from("/tmp/test.tsx"),
            &merged_refs,
            None,
            &ctx,
        );

        // All rules appear in the prompt
        assert!(prompt.contains("rule-alpha"));
        assert!(prompt.contains("rule-beta"));
        assert!(prompt.contains("rule-gamma"));
        // Uses the generic migration description
        assert!(prompt.contains("code migration"));
    }

    #[test]
    fn test_extract_text_from_goose_json_valid() {
        let json = r#"{
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "The file has been updated."}]}
            ]
        }"#;
        let result = extract_text_from_goose_json(json);
        assert_eq!(result, "The file has been updated.");
    }

    #[test]
    fn test_extract_text_from_goose_json_empty_messages() {
        let json = r#"{"messages": []}"#;
        let result = extract_text_from_goose_json(json);
        // Returns empty so retry logic can detect the failure
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_text_from_goose_json_user_only_no_assistant() {
        let json =
            r#"{"messages": [{"role": "user", "content": [{"type": "text", "text": "prompt"}]}]}"#;
        let result = extract_text_from_goose_json(json);
        // User message but no assistant response — return empty for retry
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_text_from_goose_json_empty_object() {
        let json = "{}";
        let result = extract_text_from_goose_json(json);
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_extract_text_from_goose_json_not_json() {
        let text = "This is plain text output from goose";
        let result = extract_text_from_goose_json(text);
        assert_eq!(result, text);
    }

    fn make_req_with_family(rule_id: &str, line: u32, family: &str) -> LlmFixRequest {
        let mut req = make_req_at_line(rule_id, line, None);
        req.labels.push(format!("family={}", family));
        req
    }

    #[test]
    fn test_family_first_sort_groups_families_before_non_family() {
        // Family rules should sort before non-family rules regardless
        // of individual priority. Within families, same-family rules
        // cluster together.
        let reqs = vec![
            make_req_with_family("sd-composition-alert-requires-close", 10, "Alert"),
            make_req_at_line("semver-modal-component-import-deprecated", 3, None),
            make_req_with_family("sd-composition-modal-new-member-body", 20, "Modal"),
            make_req_with_family("sd-conformance-alert-close-in-group", 15, "Alert"),
            make_req_at_line("sd-conformance-pagesection-in-page", 40, None),
            make_req_with_family("sd-composition-modal-new-member-header", 25, "Modal"),
        ];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();

        let mut merged = merge_by_rule_id(&refs);
        // Use the family-first sort (same logic as run_all_goose_fixes)
        let ctx = crate::context::GenericFixContext;
        merged.sort_by(|a, b| {
            let a_has_family = a.family.is_some();
            let b_has_family = b.family.is_some();
            match (a_has_family, b_has_family) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (true, true) => a.family.cmp(&b.family).then_with(|| {
                    ctx.fix_priority(&a.rule_id)
                        .cmp(&ctx.fix_priority(&b.rule_id))
                }),
                (false, false) => ctx
                    .fix_priority(&a.rule_id)
                    .cmp(&ctx.fix_priority(&b.rule_id)),
            }
        });

        // All Alert rules first, then all Modal rules, then non-family
        assert_eq!(merged.len(), 6);
        assert_eq!(merged[0].family.as_deref(), Some("Alert"));
        assert_eq!(merged[1].family.as_deref(), Some("Alert"));
        assert_eq!(merged[2].family.as_deref(), Some("Modal"));
        assert_eq!(merged[3].family.as_deref(), Some("Modal"));
        assert!(merged[4].family.is_none());
        assert!(merged[5].family.is_none());
    }

    #[test]
    fn test_extract_text_from_goose_json_multiple_text_blocks() {
        let json = r#"{
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "First part."},
                    {"type": "toolUse", "name": "developer__read_file"},
                    {"type": "text", "text": "Second part."}
                ]}
            ]
        }"#;
        let result = extract_text_from_goose_json(json);
        assert_eq!(result, "First part.\nSecond part.");
    }

    // ── extract_changes_applied tests ────────────────────────────────────

    #[test]
    fn test_extract_changes_applied_standard_header() {
        let response = "Some reasoning...\n\n\
            ## Changes Applied\n\
            - Moved AlertActionCloseButton from actionClose prop to child of Alert\n\
            - Added ModalHeader, ModalBody, ModalFooter imports\n";
        let result = extract_changes_applied(response);
        assert!(result.is_some());
        let section = result.unwrap();
        assert!(section.starts_with("## Changes Applied"));
        assert!(section.contains("AlertActionCloseButton"));
        assert!(section.contains("ModalHeader"));
    }

    #[test]
    fn test_extract_changes_applied_summary_of_changes_variant() {
        let response = "Analysis...\n\n\
            ## Summary of Changes\n\
            - Removed EmptyStateHeader\n\
            - Moved props to EmptyState\n";
        let result = extract_changes_applied(response);
        assert!(result.is_some());
        assert!(result.unwrap().contains("EmptyStateHeader"));
    }

    #[test]
    fn test_extract_changes_applied_no_summary() {
        let response = "The file already follows the correct pattern. No changes needed.";
        let result = extract_changes_applied(response);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_changes_applied_stops_at_next_heading() {
        let response = "## Changes Applied\n\
            - Fixed Modal composition\n\n\
            ## Additional Notes\n\
            Some extra info that should not be included.\n";
        let result = extract_changes_applied(response);
        assert!(result.is_some());
        let section = result.unwrap();
        assert!(section.contains("Fixed Modal"));
        assert!(!section.contains("Additional Notes"));
    }
}
