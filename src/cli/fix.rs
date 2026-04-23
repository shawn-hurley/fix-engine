use anyhow::Result;
use clap::{Args, ValueEnum};
use fix_engine_core::FixSource;
use std::path::PathBuf;

use fix_engine::engine;
use fix_engine::goose_client;
use fix_engine::llm_client;
use fix_engine::registry::FixContextRegistry;
use fix_engine_js_fix::JsFixProvider;

/// LLM provider for AI-assisted fixes.
#[derive(Debug, Clone, ValueEnum)]
pub enum LlmProvider {
    /// Local goose CLI (runs goose as a subprocess).
    Goose,
    /// Remote OpenAI-compatible endpoint.
    Openai,
}

/// Output format for results.
#[derive(Debug, Clone, Default, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable progress output (default).
    #[default]
    Text,
    /// Machine-readable JSON summary (for CI/CD).
    Json,
}

/// Control colored output.
#[derive(Debug, Clone, Default, ValueEnum)]
pub enum ColorMode {
    /// Detect terminal capabilities (default).
    #[default]
    Auto,
    /// Always emit color codes.
    Always,
    /// Never emit color codes.
    Never,
}

#[derive(Args)]
pub struct FixOpts {
    /// Path to the project to fix.
    pub project: PathBuf,

    /// Path to Konveyor analysis output (YAML or JSON).
    #[arg(short, long)]
    pub input: PathBuf,

    /// Preview planned changes as a unified diff without writing to disk.
    #[arg(long)]
    pub dry_run: bool,

    /// LLM provider for AI-assisted fixes.
    #[arg(long, value_enum)]
    pub llm_provider: Option<LlmProvider>,

    /// LLM endpoint URL (required when --llm-provider=openai).
    #[arg(long, required_if_eq("llm_provider", "openai"))]
    pub llm_endpoint: Option<String>,

    /// Path to fix strategies JSON file(s).
    ///
    /// Can be specified multiple times. Each file is a JSON map of
    /// rule ID -> fix strategy. Later files override earlier ones
    /// when they share the same rule ID.
    ///
    /// Example: --strategies rules/fix-strategies.json --strategies output/fix-strategies.json
    #[arg(long)]
    pub strategies: Vec<PathBuf>,

    /// Directory to save goose prompts and responses for debugging.
    #[arg(long)]
    pub log_dir: Option<PathBuf>,

    /// Show detailed output.
    #[arg(short, long)]
    pub verbose: bool,

    /// Suppress progress output; only show errors and final summary.
    #[arg(short, long)]
    pub quiet: bool,

    /// Output format for results.
    #[arg(long, value_enum, default_value_t)]
    pub output_format: OutputFormat,

    /// Control colored output.
    #[arg(long, value_enum, default_value_t)]
    pub color: ColorMode,
}

pub async fn run(opts: FixOpts, progress: &crate::progress::ProgressReporter) -> Result<()> {
    use owo_colors::OwoColorize;

    let run_start = std::time::Instant::now();
    let project = opts.project.canonicalize()?;

    // ── Load analysis output ─────────────────────────────────────────
    let phase = progress.start_phase("Loading analysis output...");
    let input_content = std::fs::read_to_string(&opts.input)?;

    let output: Vec<konveyor_core::report::RuleSet> = {
        let trimmed = input_content.trim_start();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            serde_json::from_str(&input_content)?
        } else {
            yaml_serde::from_str::<Vec<konveyor_core::report::RuleSet>>(&input_content)?
        }
    };

    let context_registry = FixContextRegistry::new();
    let ruleset_name = output.first().map(|rs| rs.name.as_str()).unwrap_or("");
    let fix_context = context_registry.get(ruleset_name);

    let total_violations: usize = output.iter().map(|rs| rs.violations.len()).sum();
    let total_incidents: usize = output
        .iter()
        .flat_map(|rs| rs.violations.values())
        .map(|v| v.incidents.len())
        .sum();
    let total_errors: usize = output.iter().map(|rs| rs.errors.len()).sum();

    phase.finish_with_detail(
        "Loaded analysis output",
        &format!("{} violations, {} incidents", total_violations, total_incidents),
    );

    if total_errors > 0 {
        progress.println(&format!(
            "\n{} Provider errors ({} rules affected)",
            "warning:".yellow().bold(),
            total_errors,
        ));
        let mut seen_errors = std::collections::HashSet::new();
        for rs in &output {
            for error_msg in rs.errors.values() {
                if seen_errors.insert(error_msg.clone()) {
                    progress.println(&format!("  {} {}", "•".dimmed(), error_msg));
                }
            }
        }
    }

    // ── Load strategies ──────────────────────────────────────────────
    let mut merged_strategies = std::collections::BTreeMap::new();
    let mut family_entries = std::collections::BTreeMap::new();
    let mut strategy_warnings = Vec::new();

    if !opts.strategies.is_empty() {
        let phase = progress.start_phase(&format!(
            "Loading strategies from {} file{}...",
            opts.strategies.len(),
            if opts.strategies.len() == 1 { "" } else { "s" },
        ));

        for strategies_path in &opts.strategies {
            match fix_engine_core::load_strategies_and_families(strategies_path) {
                Ok((strats, families)) => {
                    merged_strategies.extend(strats);
                    family_entries.extend(families);
                }
                Err(e) => {
                    strategy_warnings.push(format!(
                        "Failed to load {}: {}",
                        strategies_path.display(),
                        e,
                    ));
                }
            }
        }

        let mut detail_parts = vec![format!("{} strategies", merged_strategies.len())];
        if !family_entries.is_empty() {
            detail_parts.push(format!("{} families", family_entries.len()));
        }
        if opts.strategies.len() > 1 {
            detail_parts.push(format!("{} files merged", opts.strategies.len()));
        }

        if strategy_warnings.is_empty() {
            phase.finish_with_detail("Loaded strategies", &detail_parts.join(", "));
        } else {
            phase.finish_failed(&format!(
                "Loaded strategies with {} warning(s)",
                strategy_warnings.len(),
            ));
            for warn in &strategy_warnings {
                progress.println(&format!(
                    "  {} {}",
                    "warning:".yellow().bold(),
                    warn,
                ));
            }
        }
    }

    // ── Plan fixes ───────────────────────────────────────────────────
    let lang = JsFixProvider::new();
    let mut report = fix_engine_core::FixReport::new();

    let phase = progress.start_phase("Planning fixes...");
    let mut plan = engine::plan_fixes(&output, &project, &merged_strategies, &lang, &mut report)?;

    // Consolidate family-grouped LLM requests.
    let mut consolidated_count = 0;
    if !family_entries.is_empty() {
        let before_count = plan.pending_llm.len();
        engine::consolidate_family_requests(&mut plan.pending_llm, &family_entries);
        consolidated_count = before_count - plan.pending_llm.len();
    }

    let pattern_fix_count: usize = plan
        .files
        .values()
        .flat_map(|fixes| fixes.iter())
        .filter(|f| f.source == FixSource::Pattern)
        .count();
    let pattern_edit_count: usize = plan
        .files
        .values()
        .flat_map(|fixes| fixes.iter())
        .filter(|f| f.source == FixSource::Pattern)
        .flat_map(|f| f.edits.iter())
        .count();

    let mut plan_detail = format!(
        "{} pattern fixes ({} edits), {} LLM, {} manual",
        pattern_fix_count,
        pattern_edit_count,
        plan.pending_llm.len(),
        plan.manual.len(),
    );
    if consolidated_count > 0 {
        plan_detail.push_str(&format!(", {} family-consolidated", consolidated_count));
    }
    phase.finish_with_detail("Planned fixes", &plan_detail);

    // ── Apply / preview pattern-based fixes ──────────────────────────
    let should_apply = !opts.dry_run;

    if should_apply && !plan.files.is_empty() {
        let phase = progress.start_phase("Applying pattern-based fixes...");
        let result = engine::apply_fixes(&plan, &lang, &project)?;

        let mut detail_parts = vec![
            format!("{} files", result.files_modified),
            format!("{} edits", result.edits_applied),
        ];
        if result.edits_subsumed > 0 {
            detail_parts.push(format!("{} subsumed", result.edits_subsumed));
        }
        if !result.failed_edits.is_empty() {
            detail_parts.push(format!("{} failed", result.failed_edits.len()));
        }

        if result.errors.is_empty() {
            phase.finish_with_detail("Applied pattern fixes", &detail_parts.join(", "));
        } else {
            phase.finish_failed(&format!(
                "Applied pattern fixes with {} error(s)",
                result.errors.len(),
            ));
            for err in &result.errors {
                progress.println(&format!("    {}", err));
            }
        }
    } else if opts.dry_run {
        let diff = engine::preview_fixes(&plan, &lang)?;
        if diff.is_empty() {
            progress.println("No pattern-based auto-fixable changes found.");
        } else {
            progress.println(&format!(
                "\n{}\n",
                "Planned pattern-based changes (use without --dry-run to apply):".dimmed(),
            ));
            println!("{}", diff);
        }
    }

    // ── LLM-assisted fixes ───────────────────────────────────────────
    let llm_provider = opts.llm_provider.as_ref().or_else(|| {
        if opts.llm_endpoint.is_some() {
            Some(&LlmProvider::Openai)
        } else {
            None
        }
    });

    if !plan.pending_llm.is_empty() {
        match llm_provider {
            Some(LlmProvider::Goose) => {
                if !should_apply {
                    progress.println(&format!(
                        "\n{} {} LLM-assisted fixes planned (dry run — run without --dry-run to apply)",
                        "info:".cyan().bold(),
                        plan.pending_llm.len(),
                    ));
                    for req in &plan.pending_llm {
                        let file_name = req
                            .file_path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        progress.println(&format!(
                            "  {} {} line {} [{}]",
                            "•".dimmed(),
                            file_name,
                            req.line,
                            req.rule_id.dimmed(),
                        ));
                    }
                } else {
                    let pending = std::mem::take(&mut plan.pending_llm);
                    let printer = progress.engine_printer();
                    let results = goose_client::run_all_goose_fixes(
                        &pending,
                        fix_context,
                        opts.verbose,
                        opts.log_dir.as_deref(),
                        &printer,
                    );

                    let succeeded = results.iter().filter(|r| r.success).count();
                    let failed = results.iter().filter(|r| !r.success).count();
                    progress.println(&format!(
                        "\n  Goose fixes: {} succeeded, {} failed",
                        succeeded.green(),
                        if failed > 0 {
                            format!("{}", failed).red().to_string()
                        } else {
                            "0".to_string()
                        },
                    ));

                    for (result, requests) in results.iter().zip({
                        let mut by_file: std::collections::BTreeMap<
                            PathBuf,
                            Vec<&fix_engine_core::LlmFixRequest>,
                        > = std::collections::BTreeMap::new();
                        for req in &pending {
                            by_file.entry(req.file_path.clone()).or_default().push(req);
                        }
                        by_file.into_values().collect::<Vec<_>>()
                    }) {
                        if !result.success {
                            for req in requests {
                                report.record_skip(
                                    &req.rule_id,
                                    &req.file_uri,
                                    Some(req.line),
                                    fix_engine_core::SkipReason::GooseFailed,
                                    None,
                                );
                                plan.manual.push(fix_engine_core::ManualFixItem {
                                    rule_id: req.rule_id.clone(),
                                    file_uri: req.file_uri.clone(),
                                    line: req.line,
                                    message: req.message.clone(),
                                    code_snip: req.code_snip.clone(),
                                });
                            }
                        }
                    }
                }
            }
            Some(LlmProvider::Openai) => {
                // `--llm-endpoint` is enforced by clap's `required_if_eq`.
                let endpoint = opts.llm_endpoint.as_deref().unwrap();

                if !plan.pending_llm.is_empty() {
                    let phase = progress.start_phase(&format!(
                        "Sending {} incidents to LLM endpoint: {}",
                        plan.pending_llm.len(),
                        endpoint,
                    ));

                    let pending = std::mem::take(&mut plan.pending_llm);
                    let mut llm_fixes = 0;
                    let mut llm_errors = 0;

                    for request in &pending {
                        match llm_client::request_llm_fix(endpoint, request, fix_context).await {
                            Ok(fixes) => {
                                let has_edits = fixes.iter().any(|f| !f.edits.is_empty());
                                if has_edits {
                                    for fix in fixes {
                                        if !fix.edits.is_empty() {
                                            llm_fixes += 1;
                                            plan.files
                                                .entry(request.file_path.clone())
                                                .or_default()
                                                .push(fix);
                                        }
                                    }
                                } else {
                                    // LLM responded but produced no actionable fixes.
                                    report.record_skip(
                                        &request.rule_id,
                                        &request.file_uri,
                                        Some(request.line),
                                        fix_engine_core::SkipReason::EmptyLlmResponse,
                                        None,
                                    );
                                    plan.manual.push(fix_engine_core::ManualFixItem {
                                        rule_id: request.rule_id.clone(),
                                        file_uri: request.file_uri.clone(),
                                        line: request.line,
                                        message: request.message.clone(),
                                        code_snip: request.code_snip.clone(),
                                    });
                                }
                            }
                            Err(e) => {
                                llm_errors += 1;
                                report.record_skip(
                                    &request.rule_id,
                                    &request.file_uri,
                                    Some(request.line),
                                    fix_engine_core::SkipReason::LlmError,
                                    Some(e.to_string()),
                                );
                                if opts.verbose {
                                    progress.println(&format!(
                                        "  LLM error for {}:{} — {}",
                                        request.file_path.display(),
                                        request.line,
                                        e,
                                    ));
                                }
                                plan.manual.push(fix_engine_core::ManualFixItem {
                                    rule_id: request.rule_id.clone(),
                                    file_uri: request.file_uri.clone(),
                                    line: request.line,
                                    message: request.message.clone(),
                                    code_snip: request.code_snip.clone(),
                                });
                            }
                        }
                    }

                    phase.finish_with_detail(
                        "LLM fixes",
                        &format!("{} generated, {} errors", llm_fixes, llm_errors),
                    );

                    if should_apply && !plan.files.is_empty() {
                        let apply_phase = progress.start_phase("Applying LLM fixes...");
                        let result = engine::apply_fixes(&plan, &lang, &project)?;
                        apply_phase.finish_with_detail(
                            "Applied LLM fixes",
                            &format!("{} edits", result.edits_applied),
                        );
                    }
                }
            }
            None => {
                progress.println(&format!(
                    "\n{} {} incidents need LLM-assisted fixes.",
                    "info:".cyan().bold(),
                    plan.pending_llm.len(),
                ));
                progress.println("  Use --llm-provider goose to fix with local goose.");
                progress.println("  Use --llm-provider openai --llm-endpoint <url> for remote LLM.");

                for request in std::mem::take(&mut plan.pending_llm) {
                    plan.manual.push(fix_engine_core::ManualFixItem {
                        rule_id: request.rule_id,
                        file_uri: request.file_uri,
                        line: request.line,
                        message: request.message,
                        code_snip: request.code_snip,
                    });
                }
            }
        }
    }

    // ── Manual review items ──────────────────────────────────────────
    if !plan.manual.is_empty() {
        progress.println(&format!(
            "\n{} Manual review required ({})",
            "warning:".yellow().bold(),
            plan.manual.len(),
        ));
        for item in &plan.manual {
            let file_name = item
                .file_uri
                .strip_prefix("file://")
                .unwrap_or(&item.file_uri)
                .split('/')
                .next_back()
                .unwrap_or("?");
            progress.println(&format!(
                "  {} {} line {} [{}]",
                "•".dimmed(),
                file_name,
                item.line,
                item.rule_id.dimmed(),
            ));
            if opts.verbose {
                progress.println(&format!("    {}", item.message.dimmed()));
            }
        }
    }

    // ── Merge strategy warnings into report ────────────────────────────
    for warn in &strategy_warnings {
        report.warn("strategies", warn.clone());
    }

    // ── Skipped incidents ────────────────────────────────────────────
    // Filter out informational skips (already migrated, etc.) for the warning display.
    let real_skips: Vec<_> = report.skipped.iter()
        .filter(|s| !matches!(s.reason, fix_engine_core::SkipReason::AlreadyMigrated | fix_engine_core::SkipReason::VersionAlreadyCompatible | fix_engine_core::SkipReason::NoOpRename))
        .collect();
    let info_skips: Vec<_> = report.skipped.iter()
        .filter(|s| matches!(s.reason, fix_engine_core::SkipReason::AlreadyMigrated | fix_engine_core::SkipReason::VersionAlreadyCompatible))
        .collect();

    if !real_skips.is_empty() {
        // Build a reason frequency map for the header
        let mut reason_counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for s in &real_skips {
            *reason_counts.entry(s.reason.to_string()).or_default() += 1;
        }
        let reason_summary: Vec<String> = reason_counts.iter().map(|(r, c)| format!("{} {}", c, r)).collect();

        progress.println(&format!(
            "\n{} {} incidents skipped ({})",
            "warning:".yellow().bold(),
            real_skips.len(),
            reason_summary.join(", "),
        ));
        if opts.verbose {
            for s in &real_skips {
                let file_name = s.file.split('/').next_back().unwrap_or("?");
                let line_str = s.line.map(|l| format!(" line {}", l)).unwrap_or_default();
                let detail_str = s.detail.as_deref().map(|d| format!(" — {}", d)).unwrap_or_default();
                progress.println(&format!(
                    "  {} {}{} [{}] — {}{}",
                    "•".dimmed(),
                    file_name,
                    line_str,
                    s.rule_id.dimmed(),
                    s.reason,
                    detail_str.dimmed(),
                ));
            }
        }
    }

    if !info_skips.is_empty() && opts.verbose {
        progress.println(&format!(
            "\n{} {} incidents already migrated (skipped)",
            "info:".cyan().bold(),
            info_skips.len(),
        ));
    }

    // ── End-of-run summary ───────────────────────────────────────────
    let total_elapsed = run_start.elapsed();
    let elapsed_str = crate::progress::format_duration(total_elapsed);

    match opts.output_format {
        OutputFormat::Json => {
            let summary = serde_json::json!({
                "mode": if should_apply { "apply" } else { "dry-run" },
                "pattern_fixes": pattern_fix_count,
                "pattern_edits": pattern_edit_count,
                "files_with_fixes": plan.files.len(),
                "manual_review": plan.manual.len(),
                "manual_items": plan.manual.iter().map(|m| {
                    serde_json::json!({
                        "rule_id": m.rule_id,
                        "file": m.file_uri,
                        "line": m.line,
                        "message": m.message,
                    })
                }).collect::<Vec<_>>(),
                "skipped": &report.skipped,
                "warnings": &report.warnings,
                "elapsed_ms": total_elapsed.as_millis(),
            });
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        OutputFormat::Text => {
            progress.println(&format!(
                "\n{}", "── Summary ──────────────────────────────".dimmed()
            ));

            if pattern_fix_count > 0 || pattern_edit_count > 0 {
                let action = if should_apply { "applied" } else { "planned" };
                progress.println(&format!(
                    "  Pattern fixes:  {} {} across {} files",
                    pattern_edit_count, action, plan.files.len(),
                ));
            }
            if !real_skips.is_empty() {
                progress.println(&format!(
                    "  Skipped:        {} incidents",
                    real_skips.len(),
                ));
            }
            if !info_skips.is_empty() {
                progress.println(&format!(
                    "  Already done:   {} incidents",
                    info_skips.len(),
                ));
            }
            if !plan.manual.is_empty() {
                progress.println(&format!(
                    "  Manual review:  {} incidents remaining",
                    plan.manual.len(),
                ));
            }
            if !report.warnings.is_empty() {
                progress.println(&format!(
                    "  Warnings:       {}",
                    report.warnings.len(),
                ));
            }
            progress.println(&format!(
                "  Elapsed:        {}",
                elapsed_str,
            ));
        }
    }

    Ok(())
}
