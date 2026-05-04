//! Fix engine: maps analysis violations to concrete text edits.
//!
//! Two-tier approach:
//! 1. Pattern-based: deterministic renames/removals driven by incident variables
//! 2. LLM-assisted: complex structural changes sent to an LLM endpoint
//!
//! The engine is language-agnostic. Language-specific operations (attribute
//! removal, import deduplication, path skipping, dependency management) are
//! delegated to a [`LanguageFixProvider`](crate::language::LanguageFixProvider)
//! implementation.

use anyhow::Result;
use fix_engine_core::*;
use konveyor_core::incident::Incident;
use konveyor_core::report::RuleSet;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::language::LanguageFixProvider;

/// Build a fix plan from analysis output.
///
/// `strategies` is a merged map of rule ID -> fix strategy, loaded from one or
/// more external JSON files (rule-adjacent and/or semver-analyzer generated).
/// When no strategy is found for a rule, label-based inference is attempted,
/// falling back to LLM-assisted fixes.
///
/// `lang` provides language-specific fix operations (attribute removal,
/// matched text extraction, path skipping, dependency management).
pub fn plan_fixes(
    output: &[RuleSet],
    project_root: &std::path::Path,
    strategies: &BTreeMap<String, FixStrategy>,
    lang: &dyn LanguageFixProvider,
    report: &mut FixReport,
) -> Result<FixPlan> {
    let mut plan = FixPlan::default();

    // Pre-compute dead CSS class texts from violations labeled
    // "change-type=css-dead-class". These are classes where a naive
    // v5→v6 prefix swap produces a non-existent class. Used to suppress
    // CssVariablePrefix edits that would create broken class references.
    let dead_css_classes = collect_dead_css_classes(output);

    for ruleset in output {
        for (rule_id, violation) in &ruleset.violations {
            // Lookup order: strategies map -> label inference -> LLM fallback
            let strategy = strategies
                .get(rule_id.as_str())
                .cloned()
                .or_else(|| infer_strategy_from_labels(&violation.labels).cloned())
                .unwrap_or(FixStrategy::Llm { context: None });

            for incident in &violation.incidents {
                let file_path = uri_to_path(&incident.file_uri, project_root);

                // Let the language provider decide which paths to skip
                // (e.g., node_modules for JS/TS projects).
                if lang.should_skip_path(&file_path) {
                    continue;
                }

                match &strategy {
                    FixStrategy::Rename(mappings) => {
                        if let Some(mut fix) =
                            plan_rename(rule_id, incident, mappings, &file_path, lang, report)
                        {
                            // Import renames: the old name appears in both the
                            // import specifier AND the module path (e.g.,
                            // `import { OLD } from '.../dist/js/OLD'`). Replace
                            // all occurrences so both are updated.
                            if incident.variables.contains_key("importedName") {
                                for edit in &mut fix.edits {
                                    edit.replace_all = true;
                                }
                            }
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::RemoveAttribute => {
                        if let Some(fix) = lang.plan_remove_attribute(rule_id, incident, &file_path, report)
                        {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::ImportPathChange { old_path, new_path } => {
                        if let Some(fix) = plan_import_path_change(
                            rule_id, incident, old_path, new_path, &file_path, report,
                        ) {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::JavaImportRename {
                        ref old_fqn,
                        ref new_fqn,
                    } => {
                        if let Some(fix) = lang.plan_import_rename(
                            rule_id, incident, old_fqn, new_fqn, &file_path, report,
                        ) {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::CssVariablePrefix {
                        old_prefix,
                        new_prefix,
                        exclude_patterns,
                    } => {
                        // Check if this incident matches a dead-class exclusion.
                        // If the matched text contains an excluded pattern (a CSS
                        // class that was removed, not just prefix-renamed), skip
                        // the automated prefix swap. The dead-class rule will flag
                        // this incident for manual review separately.
                        let matched_text = incident
                            .variables
                            .get("matchingText")
                            .and_then(|v| v.as_str())
                            .or_else(|| {
                                incident.variables.get("className").and_then(|v| v.as_str())
                            })
                            .unwrap_or("");

                        if !exclude_patterns.is_empty()
                            && exclude_patterns
                                .iter()
                                .any(|excl| matched_text.contains(excl.as_str()))
                        {
                            tracing::debug!(
                                rule_id = %rule_id,
                                matched = %matched_text,
                                "Skipping CssVariablePrefix for dead class (excluded pattern)"
                            );
                            // Emit as manual review instead
                            plan.manual.push(ManualFixItem {
                                rule_id: rule_id.clone(),
                                file_uri: incident.file_uri.clone(),
                                line: incident.line_number.unwrap_or(0),
                                message: format!(
                                    "CSS class '{}' was removed — prefix swap would produce a non-existent class. {}",
                                    matched_text,
                                    incident.message
                                ),
                                code_snip: incident.code_snip.clone(),
                            });
                            continue;
                        }

                        // Check if applying this prefix swap would produce a
                        // dead CSS class (one that doesn't exist in the target
                        // version). This catches cases where a broad prefix rule
                        // (e.g., pf-v5-c-expandable-section → pf-v6-c-expandable-section)
                        // would incorrectly transform a substring of a dead class
                        // (e.g., pf-v5-c-expandable-section__toggle → non-existent
                        // pf-v6-c-expandable-section__toggle).
                        //
                        // Only check `would_produce.contains(dead)` (the result
                        // contains a dead class as a substring). Do NOT check the
                        // reverse (`dead.contains(would_produce)`) — that would
                        // falsely suppress valid shorter classes when a longer dead
                        // class happens to contain them as a prefix. For example,
                        // `pf-v6-c-wizard__footer` is valid but would be suppressed
                        // because dead class `pf-v6-c-wizard__footer-cancel`
                        // contains it as a substring.
                        if !dead_css_classes.is_empty() && !matched_text.is_empty() {
                            let would_produce =
                                matched_text.replace(old_prefix.as_str(), new_prefix.as_str());
                            if dead_css_classes.iter().any(|dead| {
                                would_produce.contains(dead.as_str())
                            }) {
                                tracing::debug!(
                                    rule_id = %rule_id,
                                    matched = %matched_text,
                                    would_produce = %would_produce,
                                    "Suppressing CssVariablePrefix — swap would produce dead class"
                                );
                                plan.manual.push(ManualFixItem {
                                    rule_id: rule_id.clone(),
                                    file_uri: incident.file_uri.clone(),
                                    line: incident.line_number.unwrap_or(0),
                                    message: format!(
                                        "CSS class '{}' swap suppressed — '{}' does not exist in the target version. {}",
                                        matched_text,
                                        would_produce,
                                        incident.message,
                                    ),
                                    code_snip: incident.code_snip.clone(),
                                });
                                continue;
                            }
                        }

                        // Treat CSS prefix changes as renames
                        let mappings = vec![RenameMapping {
                            old: old_prefix.clone(),
                            new: new_prefix.clone(),
                        }];
                        if let Some(mut fix) =
                            plan_rename(rule_id, incident, &mappings, &file_path, lang, report)
                        {
                            // CSS prefix edits should replace ALL occurrences on a line,
                            // e.g. className="pf-v5-u-color-200 pf-v5-u-font-weight-light"
                            for edit in &mut fix.edits {
                                edit.replace_all = true;
                            }
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::EnsureDependency {
                        ref package,
                        ref new_version,
                        ..
                    } => {
                        // Source file incidents (e.g., .tsx importing from the wrong
                        // package) need an import rewrite, not just a manifest update.
                        // Route them to the LLM path so goose can rewrite the import.
                        // The manifest update still happens via plan_ensure_dependency
                        // with the find_nearest_package_json fallback.
                        let ext = file_path
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("");
                        // Java source files: skip EnsureDependency entirely.
                        // The namespace migration handles imports via JavaImportRename;
                        // the dependency update targets the manifest file, not .java files.
                        if ext == "java" {
                            continue;
                        }
                        if matches!(ext, "tsx" | "ts" | "jsx" | "js") {
                            let mut enriched_message = incident.message.clone();
                            if let (Some(imported), Some(module)) = (
                                incident
                                    .variables
                                    .get("importedName")
                                    .and_then(|v| v.as_str()),
                                incident
                                    .variables
                                    .get("module")
                                    .and_then(|v| v.as_str()),
                            ) {
                                enriched_message = format!(
                                    "Import '{}' is from '{}' but needs to move to \
                                     '{}' (version {}).\n\n\
                                     Change the import source from '{}' to '{}'.\n\n{}",
                                    imported,
                                    module,
                                    package,
                                    new_version,
                                    module,
                                    package,
                                    enriched_message,
                                );
                            }
                            plan.pending_llm.push(LlmFixRequest {
                                rule_id: rule_id.clone(),
                                file_uri: incident.file_uri.clone(),
                                file_path: file_path.clone(),
                                line: incident.line_number.unwrap_or(0),
                                message: enriched_message,
                                code_snip: incident.code_snip.clone(),
                                source: None,
                                labels: vec![],
                                companion_test_files: lang
                                    .discover_companion_test_files(&file_path),
                            });
                        } else {
                            // Manifest/lockfile incidents — delegate to language
                            // provider for ecosystem-specific dependency management.
                            let fixes = lang.plan_ensure_dependency(
                                rule_id,
                                incident,
                                package,
                                new_version,
                                &file_path,
                                report,
                            );
                            for fix in fixes {
                                let dep_file = fix.file_uri.clone();
                                let dep_path = uri_to_path(&dep_file, project_root);
                                plan.files.entry(dep_path).or_default().push(fix);
                            }
                        }
                    }
                    FixStrategy::Manual => {
                        plan.manual.push(ManualFixItem {
                            rule_id: rule_id.clone(),
                            file_uri: incident.file_uri.clone(),
                            line: incident.line_number.unwrap_or(0),
                            message: incident.message.clone(),
                            code_snip: incident.code_snip.clone(),
                        });
                    }
                    FixStrategy::Llm { ref context } => {
                        let companion_test_files =
                            lang.discover_companion_test_files(&file_path);

                        // If this is a test-impact-only violation and there are
                        // no companion test files, skip the LLM session — there
                        // is nothing actionable to fix.
                        let is_test_impact_only = violation
                            .labels
                            .iter()
                            .any(|l| l == "change-type=test-impact")
                            && !violation.labels.iter().any(|l| {
                                l.starts_with("change-type=")
                                    && l != "change-type=test-impact"
                            });

                        if is_test_impact_only && companion_test_files.is_empty() {
                            tracing::debug!(
                                rule_id = %rule_id,
                                file = %file_path.display(),
                                "Skipping test-impact violation — no companion test files"
                            );
                            plan.manual.push(ManualFixItem {
                                rule_id: rule_id.clone(),
                                file_uri: incident.file_uri.clone(),
                                line: incident.line_number.unwrap_or(0),
                                message: format!(
                                    "Test-impact violation with no companion test files found. {}",
                                    incident.message,
                                ),
                                code_snip: incident.code_snip.clone(),
                            });
                            continue;
                        }

                        let mut enriched_message = incident.message.clone();

                        // Append incident variables as structured context
                        // (propName, componentName, propValue, module, etc.)
                        if !incident.variables.is_empty() {
                            enriched_message.push_str("\n\nIncident context:");
                            for (key, value) in &incident.variables {
                                let val_str = match value {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                enriched_message.push_str(&format!("\n  {}: {}", key, val_str));
                            }
                        }

                        // Append strategy context if available (from FixStrategyEntry)
                        if let Some(ctx) = context {
                            enriched_message.push_str(&format!("\n\nFix strategy:\n{}", ctx));
                        }

                        plan.pending_llm.push(LlmFixRequest {
                            rule_id: rule_id.clone(),
                            file_uri: incident.file_uri.clone(),
                            file_path: file_path.clone(),
                            line: incident.line_number.unwrap_or(0),
                            message: enriched_message,
                            code_snip: incident.code_snip.clone(),
                            source: None, // filled lazily if LLM is invoked
                            labels: violation.labels.clone(),
                            companion_test_files,
                        });
                    }
                }
            }
        }
    }

    // Proactive dependency updates: for EnsureDependency strategies that had
    // zero matching incidents (e.g., because kantra doesn't dispatch dependency
    // conditions to external providers), scan manifest files directly.
    {
        let used_dep_rule_ids: HashSet<String> = output
            .iter()
            .flat_map(|rs| rs.violations.keys())
            .cloned()
            .collect();

        for (rule_id, strategy) in strategies {
            if let FixStrategy::EnsureDependency {
                ref package,
                ref new_version,
                ref old_package,
            } = strategy
            {
                // Only run proactively if: (a) no incidents matched this rule,
                // and (b) we have an old_package to search for.
                if used_dep_rule_ids.contains(rule_id.as_str()) {
                    continue;
                }
                let old_pkg = match old_package {
                    Some(p) if !p.is_empty() => p,
                    _ => continue,
                };

                tracing::info!(
                    rule_id = %rule_id,
                    old_package = %old_pkg,
                    new_package = %package,
                    new_version = %new_version,
                    "Running proactive dependency update (no kantra incidents)"
                );

                let fixes = lang.plan_proactive_dependency(
                    rule_id, old_pkg, package, new_version, project_root, report,
                );
                for fix in fixes {
                    let dep_path = uri_to_path(&fix.file_uri, project_root);
                    plan.files.entry(dep_path).or_default().push(fix);
                }
            }
        }
    }

    // Config file FQN replacement: for JavaImportRename strategies, scan
    // non-Java config files (persistence.xml, build.gradle, *.properties,
    // *.yml, Docker configs) for FQN references and replace them. This
    // handles cases where class FQNs appear as string literals in config
    // files that the Java scanner doesn't scan (e.g., dialect references
    // like `org.hibernate.dialect.MySQL5InnoDBDialect`).
    {
        for (rule_id, strategy) in strategies {
            if let FixStrategy::JavaImportRename {
                ref old_fqn,
                ref new_fqn,
            } = strategy
            {
                // Only scan config files for FQN-style patterns (must have at least
                // 2 dots to avoid false positives from short patterns like "Criteria").
                if old_fqn.matches('.').count() < 2 {
                    continue;
                }
                let fixes = lang.plan_config_file_renames(
                    rule_id, old_fqn, new_fqn, project_root, report,
                );
                for fix in fixes {
                    let cfg_path = uri_to_path(&fix.file_uri, project_root);
                    plan.files.entry(cfg_path).or_default().push(fix);
                }
            }
        }
    }

    // Merge dependency-insert edits: when multiple EnsureDependency fixes
    // insert new packages before the same closing brace line in package.json,
    // combine them into a single multi-line insertion. Without this, only the
    // first insert succeeds — subsequent ones fail because the closing brace
    // line has already been replaced.
    merge_dependency_inserts(&mut plan);

    // Sort edits within each file by line number (descending) so we can apply bottom-up
    for fixes in plan.files.values_mut() {
        fixes.sort_by_key(|f| std::cmp::Reverse(f.line));
    }

    // Deduplicate overlapping edits: when multiple edits target the same line
    // and one edit's old_text is a substring of another's, the more specific
    // (longer old_text) edit wins. This handles the CSS rule cascade where a
    // specific rename rule, a prefix stale rule, and a class prefix rule all
    // target the same variable on the same line.
    deduplicate_edits(&mut plan);

    Ok(plan)
}

/// Remove edits that are subsumed by a more specific edit on the same line.
///
/// Two edits on the same line are considered overlapping when one's `old_text`
/// is a substring of the other's. The more specific edit (longer `old_text`)
/// wins because it produces a more precise replacement. The subsumed edit is
/// removed from the plan and counted in `plan.edits_subsumed`.
///
/// When two edits share the same `old_text` but have different `new_text`
/// Merge dependency-insert edits that target the same closing brace line.
///
/// When multiple `EnsureDependency` fixes each insert a new package before the
/// closing `}` of a dep block in package.json, they all produce edits with the
/// same `old_text` (the closing brace line) on the same line number. Only the
/// first would succeed since the closing brace is replaced. This function
/// combines them into a single edit that inserts all packages at once.
fn merge_dependency_inserts(plan: &mut FixPlan) {
    for (file_path, fixes) in plan.files.iter_mut() {
        // Only process package.json files
        if file_path
            .file_name()
            .and_then(|f| f.to_str())
            .filter(|f| *f == "package.json")
            .is_none()
        {
            continue;
        }

        // Find insert edits: description starts with "Add " and targets a
        // closing brace line. Group by (line, old_text).
        let mut insert_groups: std::collections::HashMap<(u32, String), Vec<(usize, usize)>> =
            std::collections::HashMap::new();

        for (fix_idx, fix) in fixes.iter().enumerate() {
            for (edit_idx, edit) in fix.edits.iter().enumerate() {
                if edit.description.starts_with("Add ")
                    && edit.old_text.trim().starts_with('}')
                    && edit.new_text.contains(&edit.old_text.trim().to_string())
                {
                    insert_groups
                        .entry((edit.line, edit.old_text.clone()))
                        .or_default()
                        .push((fix_idx, edit_idx));
                }
            }
        }

        // For each group with >1 insert, merge into the first edit
        for ((_line, old_text), indices) in &insert_groups {
            if indices.len() <= 1 {
                continue;
            }

            // Collect the new dependency lines from each edit.
            // Each edit's new_text is like: `    "pkg": "ver"\n  }`
            // We extract everything before the closing brace.
            let closing_trimmed = old_text.trim().to_string();
            let mut new_entries: Vec<String> = Vec::new();

            for &(fix_idx, edit_idx) in indices {
                let new_text = &fixes[fix_idx].edits[edit_idx].new_text;
                // Extract lines before the closing brace
                if let Some(pos) = new_text.rfind(&closing_trimmed) {
                    let entries_part = &new_text[..pos];
                    for entry_line in entries_part.lines() {
                        let trimmed = entry_line.trim();
                        if !trimmed.is_empty() {
                            new_entries.push(entry_line.to_string());
                        }
                    }
                }
            }

            if new_entries.is_empty() {
                continue;
            }

            // Deduplicate entries (same package might appear from multiple rules)
            let mut seen = std::collections::HashSet::new();
            new_entries.retain(|e| seen.insert(e.clone()));

            // Build the merged new_text: all entries followed by the closing brace
            // Preserve the original indentation of the closing brace line.
            let merged_new_text = format!("{}\n{}", new_entries.join(",\n"), old_text);

            // Update the first edit with the merged text
            let (first_fix, first_edit) = indices[0];
            fixes[first_fix].edits[first_edit].new_text = merged_new_text;
            fixes[first_fix].edits[first_edit].description =
                format!("Add {} dependencies to package.json", new_entries.len());

            // Remove the other edits by clearing them (they'll be deduped/skipped later)
            for &(fix_idx, edit_idx) in &indices[1..] {
                // Mark as no-op: set old_text to something that won't match
                fixes[fix_idx].edits[edit_idx].old_text =
                    "__MERGED_DEPENDENCY_INSERT__".to_string();
            }

            tracing::info!(
                file = %file_path.display(),
                count = new_entries.len(),
                "Merged dependency insert edits into single edit"
            );
        }
    }
}

/// (conflicting edits), the first in specificity order is kept.
///
/// Exact duplicates (same line, old_text, new_text) are also removed.
fn deduplicate_edits(plan: &mut FixPlan) {
    let mut total_subsumed: usize = 0;

    for fixes in plan.files.values_mut() {
        // Collect all edits across all PlannedFixes for this file, tracking
        // which PlannedFix and edit index they came from.
        struct EditRef {
            fix_idx: usize,
            edit_idx: usize,
            line: u32,
            old_text: String,
            new_text: String,
            replace_all: bool,
        }

        let mut all_edits: Vec<EditRef> = Vec::new();
        for (fix_idx, fix) in fixes.iter().enumerate() {
            for (edit_idx, edit) in fix.edits.iter().enumerate() {
                all_edits.push(EditRef {
                    fix_idx,
                    edit_idx,
                    line: edit.line,
                    old_text: edit.old_text.clone(),
                    new_text: edit.new_text.clone(),
                    replace_all: edit.replace_all,
                });
            }
        }

        // Group by line number
        let mut by_line: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, er) in all_edits.iter().enumerate() {
            by_line.entry(er.line).or_default().push(i);
        }

        // For each line, determine which edits to remove
        let mut remove_set: std::collections::HashSet<(usize, usize)> =
            std::collections::HashSet::new();

        for indices in by_line.values() {
            if indices.len() <= 1 {
                continue;
            }

            // Sort by old_text length descending (most specific first),
            // then by old_text alphabetically for deterministic ordering
            let mut sorted: Vec<usize> = indices.clone();
            sorted.sort_by(|&a, &b| {
                let ea = &all_edits[a];
                let eb = &all_edits[b];
                eb.old_text
                    .len()
                    .cmp(&ea.old_text.len())
                    .then_with(|| ea.old_text.cmp(&eb.old_text))
            });

            // Walk in specificity order. Keep the first edit for each
            // non-overlapping text region. Subsume edits whose old_text
            // is a substring of a kept edit's old_text, or whose old_text
            // matches a kept edit's old_text (conflict: first wins).
            let mut kept: Vec<usize> = Vec::new();
            let mut kept_old_texts: Vec<String> = Vec::new();
            // Track (old_text) already seen to handle same-old-text conflicts
            let mut seen_old: std::collections::HashSet<String> = std::collections::HashSet::new();

            for &idx in &sorted {
                let er = &all_edits[idx];

                // Exact duplicate: same old_text AND same new_text as a kept edit
                let dominated = kept.iter().any(|&k| {
                    let ek = &all_edits[k];
                    ek.old_text == er.old_text && ek.new_text == er.new_text
                });
                if dominated {
                    remove_set.insert((er.fix_idx, er.edit_idx));
                    total_subsumed += 1;
                    continue;
                }

                // Conflict: same old_text but different new_text — first wins
                if seen_old.contains(&er.old_text) {
                    remove_set.insert((er.fix_idx, er.edit_idx));
                    total_subsumed += 1;
                    continue;
                }

                // Subsumed: this edit's old_text is a substring of a kept edit's
                // old_text.  However, when BOTH the kept edit and the candidate
                // use `replace_all`, they can safely coexist: the longer edit is
                // applied first (via `str::replace`), removing any overlapping
                // occurrences, and the shorter edit then catches remaining
                // independent occurrences on the same line.
                //
                // Example:
                //   className="pf-v5-c-form__label pf-v5-c-form__label-text"
                //   Edit A (kept): "pf-v5-c-form__label-text" → "pf-v6-c-form__label-text"
                //   Edit B (this): "pf-v5-c-form__label"      → "pf-v6-c-form__label"
                //   Both replace_all=true → B survives to fix the standalone occurrence.
                let subsumed = kept.iter().any(|&k| {
                    let ek = &all_edits[k];
                    ek.old_text.contains(&er.old_text)
                        && !(er.replace_all && ek.replace_all)
                });
                if subsumed {
                    remove_set.insert((er.fix_idx, er.edit_idx));
                    total_subsumed += 1;
                    continue;
                }

                // Keep this edit
                kept.push(idx);
                kept_old_texts.push(er.old_text.clone());
                seen_old.insert(er.old_text.clone());
            }
        }

        // Remove subsumed edits from PlannedFixes (iterate in reverse to preserve indices)
        if !remove_set.is_empty() {
            for (fix_idx, fix) in fixes.iter_mut().enumerate() {
                let mut edit_idx = fix.edits.len();
                while edit_idx > 0 {
                    edit_idx -= 1;
                    if remove_set.contains(&(fix_idx, edit_idx)) {
                        fix.edits.remove(edit_idx);
                    }
                }
            }
            // Remove PlannedFixes that have no remaining edits
            fixes.retain(|f| !f.edits.is_empty());
        }
    }

    plan.edits_subsumed = total_subsumed;
}

/// Consolidate LLM fix requests by component family when a family-level
/// strategy exists. Multiple rules targeting the same `(file, family)` are
/// merged into a single request with a unified message containing the target
/// component structure and all incident variables.
///
/// Requests without a `family=` label, or whose family has no entry in
/// `family_entries`, are left untouched.
pub fn consolidate_family_requests(
    requests: &mut Vec<LlmFixRequest>,
    family_entries: &BTreeMap<String, FixStrategyEntry>,
) {
    use std::collections::BTreeSet;

    if family_entries.is_empty() {
        return;
    }

    // Extract family label from request labels (e.g., "family=Modal" -> "Modal").
    fn extract_family(labels: &[String]) -> Option<String> {
        labels
            .iter()
            .find(|l| l.starts_with("family="))
            .and_then(|l| l.strip_prefix("family="))
            .map(|s| s.to_string())
    }

    // Group indices by (file_path, family) where a family strategy exists.
    let mut groups: BTreeMap<(PathBuf, String), Vec<usize>> = BTreeMap::new();
    let mut ungrouped_indices: BTreeSet<usize> = BTreeSet::new();

    for (idx, req) in requests.iter().enumerate() {
        if let Some(family) = extract_family(&req.labels) {
            let key = format!("family:{}", family);
            if family_entries.contains_key(&key) {
                groups
                    .entry((req.file_path.clone(), family))
                    .or_default()
                    .push(idx);
                continue;
            }
        }
        ungrouped_indices.insert(idx);
    }

    if groups.is_empty() {
        return;
    }

    // Build consolidated requests and collect indices to remove.
    let mut consolidated: Vec<LlmFixRequest> = Vec::new();
    let mut consumed_indices: BTreeSet<usize> = BTreeSet::new();

    for ((file_path, family), indices) in &groups {
        let key = format!("family:{}", family);
        let entry = &family_entries[&key];

        // Build the family migration context header.
        let mut message = format!("## {} Family Migration\n", family);

        if let Some(ref target) = entry.target_structure {
            message.push_str(&format!(
                "\nTarget structure (correct v6 composition):\n```jsx\n{}\n```\n",
                target
            ));
        }
        if !entry.retained_props.is_empty() {
            message.push_str(&format!(
                "\nProps that stay on <{}>: {}\n",
                family,
                entry.retained_props.join(", ")
            ));
        }
        if !entry.prop_to_child.is_empty() {
            message.push_str("\nProps that move to child components:\n");
            for (prop, child) in &entry.prop_to_child {
                message.push_str(&format!("  {} -> <{} />\n", prop, child));
            }
        }
        if !entry.unmapped_removed_props.is_empty() {
            message.push_str("\nRemoved props (move to child component as children or remove):\n");
            for (prop, target) in &entry.unmapped_removed_props {
                message.push_str(&format!("  {} -> {}\n", prop, target));
            }
        }
        if !entry.child_props_to_parent.is_empty() {
            message.push_str("\nChild props that move to parent:\n");
            for (child_prop, parent_prop) in &entry.child_props_to_parent {
                message.push_str(&format!("  {} -> {}\n", child_prop, parent_prop));
            }
        }
        if !entry.removed_children.is_empty() {
            message.push_str(&format!(
                "\nRemoved children (no longer valid JSX): {}\n",
                entry.removed_children.join(", ")
            ));
        }
        if !entry.new_imports.is_empty() {
            let src = entry.import_source.as_deref().unwrap_or("(same package)");
            message.push_str(&format!(
                "\nAdd imports: {} from '{}'\n",
                entry.new_imports.join(", "),
                src
            ));
        }
        if !entry.removed_imports.is_empty() {
            message.push_str(&format!(
                "\nRemove imports (if no longer used): {}\n",
                entry.removed_imports.join(", ")
            ));
        }
        if !entry.prop_value_changes.is_empty() {
            message.push_str("\nProp value changes:\n");
            for (prop, mappings) in &entry.prop_value_changes {
                for m in mappings {
                    if let (Some(from), Some(to)) = (&m.from, &m.to) {
                        message.push_str(&format!("  {}: {} -> {}\n", prop, from, to));
                    }
                }
            }
        }
        if !entry.prop_type_changes.is_empty() {
            message.push_str("\nProp type changes:\n");
            for (prop, mappings) in &entry.prop_type_changes {
                for m in mappings {
                    match (&m.from, &m.to) {
                        (Some(from), Some(to)) => {
                            message.push_str(&format!("  {}: {} -> {}\n", prop, from, to));
                        }
                        (None, Some(to)) => {
                            message.push_str(&format!(
                                "  {} (current signature): {}\n",
                                prop, to
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(ref dm) = entry.deprecated_migration {
            message.push_str(&format!(
                "\nDeprecated -> v6 migration:\n  Old import: {}\n  New import: {}\n",
                dm.old_package, dm.new_package
            ));
            if !dm.matching_props.is_empty() {
                message.push_str("Matching props:\n");
                for p in &dm.matching_props {
                    if p.type_changed {
                        message.push_str(&format!(
                            "  {} -> {} (TYPE CHANGED):\n    old: {}\n    new: {}\n",
                            p.old_name,
                            p.new_name,
                            p.old_type.as_deref().unwrap_or("?"),
                            p.new_type.as_deref().unwrap_or("?")
                        ));
                    } else if p.old_name != p.new_name {
                        message.push_str(&format!(
                            "  {} -> {} (renamed, type unchanged)\n",
                            p.old_name, p.new_name
                        ));
                    }
                }
            }
            if !dm.new_props.is_empty() {
                message.push_str("New props on replacement:\n");
                for (name, typ) in &dm.new_props {
                    message.push_str(&format!("  {}: {}\n", name, typ));
                }
            }
            if !dm.removed_props.is_empty() {
                message.push_str(&format!(
                    "Removed props (no replacement equivalent): {}\n",
                    dm.removed_props.join(", ")
                ));
            }
        }

        // Collect all incident data from the grouped requests.
        let filtered_indices: Vec<usize> = {
            let filtered: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|&idx| {
                    let req = &requests[idx];
                    let dominated = req.labels.iter().any(|l| {
                        l == "change-type=signature-changed" || l == "change-type=type-changed"
                    }) && (req.message.contains("base class changed")
                        || req.message.contains("RefAttributes"));
                    !dominated
                })
                .collect();
            if filtered.is_empty() {
                indices.clone()
            } else {
                filtered
            }
        };

        message.push_str("\nIncidents found in this file:\n");
        let mut all_lines: Vec<u32> = Vec::new();
        let mut all_snips: Vec<(u32, String)> = Vec::new();
        let mut all_labels: Vec<String> = Vec::new();
        let mut seen_rules: BTreeSet<String> = BTreeSet::new();

        for &idx in &filtered_indices {
            let req = &requests[idx];
            all_lines.push(req.line);

            let rule_info = if seen_rules.insert(req.rule_id.clone()) {
                format!("  Line {}: [{}]\n", req.line, req.rule_id)
            } else {
                format!("  Line {}: (same rule {})\n", req.line, req.rule_id)
            };
            message.push_str(&rule_info);

            if let Some(var_start) = req.message.find("\n\nIncident context:") {
                let var_section =
                    if let Some(strat_start) = req.message[var_start..].find("\n\nFix strategy:") {
                        &req.message[var_start..var_start + strat_start]
                    } else {
                        &req.message[var_start..]
                    };
                for line in var_section.trim().lines().skip(1) {
                    message.push_str(&format!("    {}\n", line.trim()));
                }
            }

            // Preserve per-incident fix strategy context (e.g., PropTypeChange
            // from/to mappings) so the LLM knows exactly what rename or type
            // change to apply alongside the family migration.
            if let Some(strat_start) = req.message.find("\n\nFix strategy:") {
                let strat_section = &req.message[strat_start..];
                message.push_str("    Fix strategy:\n");
                for line in strat_section.trim().lines().skip(1) {
                    message.push_str(&format!("      {}\n", line.trim()));
                }
            }

            if let Some(snip) = &req.code_snip {
                all_snips.push((req.line, snip.clone()));
            }
            for label in &req.labels {
                if !all_labels.contains(label) {
                    all_labels.push(label.clone());
                }
            }
        }

        let first_line = all_lines.iter().copied().min().unwrap_or(0);
        let first_uri = requests[indices[0]].file_uri.clone();

        if !all_snips.is_empty() {
            message.push_str("\nCode contexts:\n");
            let mut seen_snips: BTreeSet<String> = BTreeSet::new();
            for (line, snip) in &all_snips {
                if seen_snips.insert(snip.clone()) {
                    message.push_str(&format!("  (line {}):\n{}\n", line, snip));
                }
            }
        }

        // Aggregate companion test files from all family member requests
        let mut family_test_files: Vec<PathBuf> = Vec::new();
        for &idx in indices {
            family_test_files.extend(requests[idx].companion_test_files.clone());
        }
        family_test_files.sort();
        family_test_files.dedup();

        consolidated.push(LlmFixRequest {
            rule_id: format!("family:{}", family),
            file_uri: first_uri,
            file_path: file_path.clone(),
            line: first_line,
            message,
            code_snip: None,
            source: None,
            labels: all_labels,
            companion_test_files: family_test_files,
        });

        for &idx in indices {
            consumed_indices.insert(idx);
        }
    }

    // Rebuild the request list: ungrouped first, then consolidated.
    let mut new_requests: Vec<LlmFixRequest> = Vec::new();
    for (idx, req) in requests.drain(..).enumerate() {
        if !consumed_indices.contains(&idx) {
            new_requests.push(req);
        }
    }
    new_requests.extend(consolidated);
    *requests = new_requests;
}

/// Generate dedicated test-fix LLM requests for companion test files.
///
/// When a component file has both:
/// 1. Test-impact violations (labels contain `change-type=test-impact`)
/// 2. Companion test files discovered by the language provider
///
/// ...this function creates a separate `LlmFixRequest` targeting each test
/// file directly. The test-fix request carries the full context of what
/// changed in the component (from the test-impact violation message) so the
/// LLM can apply the correct fix to the test.
///
/// These requests get their own Goose sessions, ensuring test files receive
/// focused attention rather than being buried as a footnote in a component
/// fix session with 7+ other changes.
pub fn generate_test_fix_requests(requests: &[LlmFixRequest]) -> Vec<LlmFixRequest> {
    let mut test_requests: Vec<LlmFixRequest> = Vec::new();
    let mut seen_test_files: HashSet<PathBuf> = HashSet::new();

    for req in requests {
        // Only generate test-fix requests from test-impact violations
        let has_test_impact = req
            .labels
            .iter()
            .any(|l| l == "change-type=test-impact");
        if !has_test_impact {
            continue;
        }

        // Must have companion test files
        if req.companion_test_files.is_empty() {
            continue;
        }

        for test_file in &req.companion_test_files {
            // Deduplicate: only create one request per test file even if
            // multiple violations reference the same companion test
            if !seen_test_files.insert(test_file.clone()) {
                continue;
            }

            let test_file_name = test_file
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("test file");

            let component_file_name = req
                .file_path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("component");

            let message = format!(
                "The component file {component_file} was modified with migration changes that \
                 may break this test file.\n\n\
                 Component changes that affect tests:\n\
                 {original_message}\n\n\
                 Instructions:\n\
                 1. Read this test file at {test_path}\n\
                 2. Read the component file at {component_path} to see the current (already migrated) code\n\
                 3. For each test that interacts with the migrated component, check whether \
                    the changes described above would cause the test to fail. If so, fix the test.\n\
                 4. Write the fixed test file",
                component_file = component_file_name,
                original_message = req.message,
                test_path = test_file.display(),
                component_path = req.file_path.display(),
            );

            test_requests.push(LlmFixRequest {
                rule_id: format!("test-fix:{}", req.rule_id),
                file_uri: format!("file://{}", test_file.display()),
                file_path: test_file.clone(),
                line: 1,
                message,
                code_snip: None,
                source: None,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=test-fix".into(),
                    "impact=frontend-testing".into(),
                ],
                companion_test_files: Vec::new(), // no further nesting
            });

            tracing::info!(
                test_file = %test_file.display(),
                component_file = %req.file_path.display(),
                rule_id = %req.rule_id,
                "Generated dedicated test-fix request for {}", test_file_name,
            );
        }
    }

    test_requests
}

/// Collect CSS class texts from violations labeled `change-type=css-dead-class`.
///
/// These are CSS classes where a naive version prefix swap (e.g.,
/// `pf-v5-c-expandable-section__toggle` → `pf-v6-c-expandable-section__toggle`)
/// produces a class that does NOT exist in the target CSS distribution.
/// Used to suppress `CssVariablePrefix` edits that would create broken references.
fn collect_dead_css_classes(output: &[RuleSet]) -> std::collections::HashSet<String> {
    let mut dead = std::collections::HashSet::new();
    for rs in output {
        for violation in rs.violations.values() {
            let is_dead = violation
                .labels
                .iter()
                .any(|l| l == "change-type=css-dead-class");
            if !is_dead {
                continue;
            }
            for inc in &violation.incidents {
                // Collect the matched text or class name from the incident.
                // The dead-class rules may use either variable depending on
                // whether the scanner matched a className or a matchingText.
                if let Some(mt) = inc
                    .variables
                    .get("matchingText")
                    .and_then(|v| v.as_str())
                {
                    dead.insert(mt.to_string());
                }
                if let Some(cn) = inc.variables.get("className").and_then(|v| v.as_str()) {
                    dead.insert(cn.to_string());
                }
            }
        }
    }
    dead
}

/// Apply a fix plan to disk.
///
/// `lang` provides language-specific post-processing (e.g., import deduplication).
/// After all files are written, calls `lang.post_apply()` for ecosystem-specific
/// steps (e.g., `npm install` after `package.json` changes).
pub fn apply_fixes(
    plan: &FixPlan,
    lang: &dyn LanguageFixProvider,
    project_root: &Path,
) -> Result<FixResult> {
    // Capture baseline state before any edits (e.g., pre-existing unmet peer deps)
    let pre_state = lang.pre_apply(project_root);

    let mut result = FixResult {
        edits_subsumed: plan.edits_subsumed,
        ..FixResult::default()
    };

    for (file_path, fixes) in &plan.files {
        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: {}", file_path.display(), e));
                continue;
            }
        };

        let mut lines: Vec<String> = source.lines().map(String::from).collect();
        let mut any_changed = false;

        let mut seen_edits: std::collections::HashSet<(u32, String, String)> =
            std::collections::HashSet::new();

        for fix in fixes {
            for edit in &fix.edits {
                let key = (edit.line, edit.old_text.clone(), edit.new_text.clone());
                if !seen_edits.insert(key) {
                    continue;
                }
                let idx = (edit.line as usize).saturating_sub(1);
                if idx < lines.len() {
                    let line = &lines[idx];
                    if line.contains(&edit.old_text) {
                        lines[idx] = if edit.replace_all {
                            line.replace(&edit.old_text, &edit.new_text)
                        } else {
                            line.replacen(&edit.old_text, &edit.new_text, 1)
                        };
                        result.edits_applied += 1;
                        any_changed = true;
                    } else {
                        tracing::debug!(
                            file = %file_path.display(),
                            line = edit.line,
                            rule = %edit.rule_id,
                            old_text = %edit.old_text,
                            actual_line = %line,
                            "Edit skipped: old_text not found on line"
                        );
                        result.failed_edits.push(FailedEdit {
                            file: file_path.clone(),
                            line: edit.line,
                            rule_id: edit.rule_id.clone(),
                            old_text: edit.old_text.clone(),
                            reason: FailedEditReason::TextNotFoundOnLine {
                                actual_line: line.to_string(),
                            },
                        });
                    }
                } else {
                    tracing::debug!(
                        file = %file_path.display(),
                        line = edit.line,
                        rule = %edit.rule_id,
                        old_text = %edit.old_text,
                        total_lines = lines.len(),
                        "Edit skipped: line index out of bounds"
                    );
                    result.failed_edits.push(FailedEdit {
                        file: file_path.clone(),
                        line: edit.line,
                        rule_id: edit.rule_id.clone(),
                        old_text: edit.old_text.clone(),
                        reason: FailedEditReason::LineOutOfBounds {
                            total_lines: lines.len(),
                        },
                    });
                }
            }
        }

        if any_changed {
            lang.post_process_lines(&mut lines);
            lines.retain(|_l| true);

            let mut output = lines.join("\n");
            if source.ends_with('\n') {
                output.push('\n');
            }
            std::fs::write(file_path, output)?;
            result.files_modified += 1;
            result.modified_files.push(file_path.clone());
        }
    }

    // Run post-apply hook (e.g., npm install after package.json changes)
    if let Err(e) = lang.post_apply(project_root, &result.modified_files, pre_state) {
        tracing::warn!("Post-apply hook failed: {}", e);
        result.errors.push(format!("Post-apply hook failed: {}", e));
    }

    Ok(result)
}

/// Generate a unified diff preview of the planned changes.
///
/// `lang` provides language-specific post-processing (e.g., import deduplication).
pub fn preview_fixes(plan: &FixPlan, lang: &dyn LanguageFixProvider) -> Result<String> {
    let mut output = String::new();

    for (file_path, fixes) in &plan.files {
        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let lines: Vec<&str> = source.lines().collect();
        let mut changed_lines: HashMap<usize, String> = HashMap::new();

        for fix in fixes {
            for edit in &fix.edits {
                let idx = (edit.line as usize).saturating_sub(1);
                if idx < lines.len() {
                    let current = changed_lines
                        .get(&idx)
                        .map(String::as_str)
                        .unwrap_or(lines[idx]);
                    if current.contains(&edit.old_text) {
                        let new_line = if edit.replace_all {
                            current.replace(&edit.old_text, &edit.new_text)
                        } else {
                            current.replacen(&edit.old_text, &edit.new_text, 1)
                        };
                        changed_lines.insert(idx, new_line);
                    }
                }
            }
        }

        if changed_lines.is_empty() {
            continue;
        }

        for (_, line_content) in changed_lines.iter_mut() {
            let mut single = [line_content.clone()];
            lang.post_process_lines(&mut single);
            *line_content = single.into_iter().next().unwrap();
        }

        output.push_str(&format!(
            "--- a/{}\n+++ b/{}\n",
            file_path.display(),
            file_path.display()
        ));

        let mut changed_indices: Vec<usize> = changed_lines.keys().copied().collect();
        changed_indices.sort();

        for &idx in &changed_indices {
            let context = 3;
            let start = idx.saturating_sub(context);
            let end = (idx + context + 1).min(lines.len());

            output.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                start + 1,
                end - start,
                start + 1,
                end - start
            ));

            for (i, line) in lines.iter().enumerate().take(end).skip(start) {
                if let Some(new_line) = changed_lines.get(&i) {
                    output.push_str(&format!("-{}\n", line));
                    output.push_str(&format!("+{}\n", new_line));
                } else {
                    output.push_str(&format!(" {}\n", line));
                }
            }
        }
    }

    Ok(output)
}

// -- Pattern-based fix generators --

fn plan_rename(
    rule_id: &str,
    incident: &Incident,
    mappings: &[RenameMapping],
    file_path: &PathBuf,
    lang: &dyn LanguageFixProvider,
    report: &mut FixReport,
) -> Option<PlannedFix> {
    let line = match incident.line_number {
        Some(l) => l,
        None => {
            report.record_skip(rule_id, &incident.file_uri, None, SkipReason::NoLineNumber, None);
            return None;
        }
    };

    let matched_text = lang.get_matched_text_for_rename(incident, mappings);
    let is_whole_file_rename = lang.is_whole_file_rename(incident);
    let primary_mapping = mappings.iter().find(|m| m.old == matched_text);

    let source = match std::fs::read_to_string(file_path) {
        Ok(s) => s,
        Err(e) => {
            report.record_skip(
                rule_id,
                &incident.file_uri,
                Some(line),
                SkipReason::FileUnreadable,
                Some(e.to_string()),
            );
            return None;
        }
    };
    let mut edits = Vec::new();

    if is_whole_file_rename {
        let mut sorted_mappings: Vec<&RenameMapping> =
            mappings.iter().filter(|m| m.old != m.new).collect();
        sorted_mappings.sort_by_key(|m| std::cmp::Reverse(m.old.len()));

        for (idx, file_line) in source.lines().enumerate() {
            let line_num = (idx + 1) as u32;
            let mut consumed: Vec<&str> = Vec::new();
            for m in &sorted_mappings {
                if file_line.contains(m.old.as_str()) {
                    let is_substring_of_consumed =
                        consumed.iter().any(|c| c.contains(m.old.as_str()));
                    if is_substring_of_consumed {
                        continue;
                    }
                    edits.push(TextEdit {
                        line: line_num,
                        old_text: m.old.clone(),
                        new_text: m.new.clone(),
                        rule_id: rule_id.to_string(),
                        description: format!("Rename '{}' to '{}'", m.old, m.new),
                        replace_all: false,
                    });
                    consumed.push(&m.old);
                }
            }
        }
    } else if let Some(mapping) = primary_mapping {
        if mapping.old == mapping.new {
            report.record_skip(rule_id, &incident.file_uri, Some(line), SkipReason::NoOpRename, None);
            return None;
        }
        edits.push(TextEdit {
            line,
            old_text: mapping.old.clone(),
            new_text: mapping.new.clone(),
            rule_id: rule_id.to_string(),
            description: format!("Rename '{}' to '{}'", mapping.old, mapping.new),
            replace_all: false,
        });

        let line_idx = (line as usize).saturating_sub(1);
        let scan_start = line_idx.saturating_sub(3);
        let scan_end = (line_idx + 5).min(source.lines().count());
        for (idx, file_line) in source
            .lines()
            .enumerate()
            .skip(scan_start)
            .take(scan_end - scan_start)
        {
            let line_num = (idx + 1) as u32;
            for m in mappings {
                if m.old == m.new {
                    continue;
                }
                if std::ptr::eq(m, mapping) && line_num == line {
                    continue;
                }
                if file_line.contains(&m.old) {
                    edits.push(TextEdit {
                        line: line_num,
                        old_text: m.old.clone(),
                        new_text: m.new.clone(),
                        rule_id: rule_id.to_string(),
                        description: format!("Rename '{}' to '{}'", m.old, m.new),
                        replace_all: false,
                    });
                }
            }
        }
    } else {
        // Fallback: scan a window around the incident line. The reported line
        // may be slightly off (e.g., scanner reports the start of a multi-line
        // template literal, but the match is a few lines later).
        let line_idx = (line as usize).saturating_sub(1);
        let scan_start = line_idx.saturating_sub(1);
        let scan_end = (line_idx + 5).min(source.lines().count());
        for (idx, file_line) in source
            .lines()
            .enumerate()
            .skip(scan_start)
            .take(scan_end - scan_start)
        {
            let line_num = (idx + 1) as u32;
            for m in mappings {
                if m.old == m.new {
                    continue;
                }
                if file_line.contains(&m.old) {
                    edits.push(TextEdit {
                        line: line_num,
                        old_text: m.old.clone(),
                        new_text: m.new.clone(),
                        rule_id: rule_id.to_string(),
                        description: format!("Rename '{}' to '{}'", m.old, m.new),
                        replace_all: false,
                    });
                }
            }
        }
    }

    if edits.is_empty() {
        report.record_skip(
            rule_id,
            &incident.file_uri,
            Some(line),
            SkipReason::TextNotFound,
            Some(format!("none of the rename mappings matched text on line {}", line)),
        );
        return None;
    }

    let desc = edits
        .iter()
        .map(|e| format!("'{}' -> '{}'", e.old_text, e.new_text))
        .collect::<Vec<_>>()
        .join(", ");

    Some(PlannedFix {
        edits,
        confidence: FixConfidence::Exact,
        source: FixSource::Pattern,
        rule_id: rule_id.to_string(),
        file_uri: incident.file_uri.clone(),
        line,
        description: format!("Rename {}", desc),
    })
}

fn plan_import_path_change(
    rule_id: &str,
    incident: &Incident,
    old_path: &str,
    new_path: &str,
    file_path: &PathBuf,
    report: &mut FixReport,
) -> Option<PlannedFix> {
    let line = match incident.line_number {
        Some(l) => l,
        None => {
            report.record_skip(rule_id, &incident.file_uri, None, SkipReason::NoLineNumber, None);
            return None;
        }
    };

    let source = match std::fs::read_to_string(file_path) {
        Ok(s) => s,
        Err(e) => {
            report.record_skip(rule_id, &incident.file_uri, Some(line), SkipReason::FileUnreadable, Some(e.to_string()));
            return None;
        }
    };
    let lines: Vec<&str> = source.lines().collect();
    let idx = (line as usize).saturating_sub(1);
    let file_line = match lines.get(idx) {
        Some(l) => l,
        None => {
            report.record_skip(rule_id, &incident.file_uri, Some(line), SkipReason::LineOutOfBounds,
                Some(format!("file has {} lines", lines.len())));
            return None;
        }
    };

    // Determine which line contains the import path to rewrite.
    //
    // For single-line imports the incident line itself has the path:
    //   import { Chart } from '@patternfly/react-charts';
    //
    // For multi-line imports the incident fires on a specifier line while
    // the package path lives on the `from` line further down:
    //   import {
    //     Chart,          <-- incident line
    //     ChartAxis,
    //   } from '@patternfly/react-charts';   <-- old_path is here
    //
    // Strategy: if the incident line doesn't contain old_path, scan backwards
    // for `{` or `import` to confirm we're inside an import block, then scan
    // forward for the `from` clause that holds the package path.
    let target_line = if file_line.contains(old_path) {
        // Single-line import (or the incident already points at the from-line).
        // Guard against double-application: if new_path is already present
        // (e.g. old_path is a prefix of new_path), skip.
        if file_line.contains(new_path) {
            report.record_skip(rule_id, &incident.file_uri, Some(line), SkipReason::AlreadyMigrated, None);
            return None;
        }
        line
    } else {
        // Multi-line import: verify we are inside an import/export block
        // by scanning backwards (up to 50 lines) for `{` or `import`.
        let in_import_block = (0..idx).rev().take(50).any(|i| {
            let l = lines[i];
            l.contains('{')
                || l.trim_start().starts_with("import")
                || l.trim_start().starts_with("export")
        });
        if !in_import_block {
            report.record_skip(rule_id, &incident.file_uri, Some(line), SkipReason::NotInImportBlock, None);
            return None;
        }

        // Scan forward from the incident line for the `from` clause.
        let from_idx = match (idx..lines.len()).take(50).find(|&i| {
            let trimmed = lines[i].trim_start();
            trimmed.starts_with("} from ")
                || trimmed.starts_with("from ")
                || (trimmed.contains(" from '") || trimmed.contains(" from \""))
        }) {
            Some(i) => i,
            None => {
                report.record_skip(rule_id, &incident.file_uri, Some(line), SkipReason::TextNotFound,
                    Some("could not find 'from' clause in import block".to_string()));
                return None;
            }
        };

        let from_line = lines[from_idx];
        if !from_line.contains(old_path) {
            report.record_skip(rule_id, &incident.file_uri, Some(line), SkipReason::TextNotFound,
                Some(format!("from clause does not contain '{}'", old_path)));
            return None;
        }
        // Guard against double-application on the from-line.
        if from_line.contains(new_path) {
            report.record_skip(rule_id, &incident.file_uri, Some(line), SkipReason::AlreadyMigrated, None);
            return None;
        }

        (from_idx + 1) as u32 // convert to 1-indexed
    };

    Some(PlannedFix {
        edits: vec![TextEdit {
            line: target_line,
            old_text: old_path.to_string(),
            new_text: new_path.to_string(),
            rule_id: rule_id.to_string(),
            description: format!("Change import path '{}' -> '{}'", old_path, new_path),
            replace_all: false,
        }],
        confidence: FixConfidence::Exact,
        source: FixSource::Pattern,
        rule_id: rule_id.to_string(),
        file_uri: incident.file_uri.clone(),
        line: target_line,
        description: format!("Change import path to '{}'", new_path),
    })
}

// -- Helpers --

/// Convert a file:// URI to a filesystem path, relative to project root.
fn uri_to_path(uri: &str, project_root: &std::path::Path) -> PathBuf {
    let path_str = uri.strip_prefix("file://").unwrap_or(uri);

    let path = PathBuf::from(path_str);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

/// Try to infer a fix strategy from rule labels when no explicit mapping exists.
/// This is a fallback for rules not covered by any strategy file.
fn infer_strategy_from_labels(labels: &[String]) -> Option<&'static FixStrategy> {
    for label in labels {
        match label.as_str() {
            "change-type=prop-removal" => return Some(&FixStrategy::RemoveAttribute),
            "change-type=dom-structure"
            | "change-type=behavioral"
            | "change-type=accessibility"
            | "change-type=interface-removal"
            | "change-type=module-export"
            | "change-type=other" => return Some(&FixStrategy::Manual),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- uri_to_path tests --

    #[test]
    fn test_uri_to_path_absolute() {
        let path = uri_to_path(
            "file:///home/user/project/src/App.tsx",
            std::path::Path::new("/ignored"),
        );
        assert_eq!(path, PathBuf::from("/home/user/project/src/App.tsx"));
    }

    #[test]
    fn test_uri_to_path_relative() {
        let path = uri_to_path("src/App.tsx", std::path::Path::new("/home/user/project"));
        assert_eq!(path, PathBuf::from("/home/user/project/src/App.tsx"));
    }

    #[test]
    fn test_uri_to_path_no_file_prefix() {
        let path = uri_to_path("/absolute/path.tsx", std::path::Path::new("/root"));
        assert_eq!(path, PathBuf::from("/absolute/path.tsx"));
    }

    // -- infer_strategy_from_labels tests --

    #[test]
    fn test_infer_prop_removal() {
        let labels = vec!["change-type=prop-removal".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::RemoveAttribute)));
    }

    #[test]
    fn test_infer_dom_structure_manual() {
        let labels = vec!["change-type=dom-structure".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_unknown_label_returns_none() {
        let labels = vec!["change-type=rename".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(strategy.is_none());
    }

    #[test]
    fn test_infer_empty_labels_returns_none() {
        let labels: Vec<String> = Vec::new();
        let strategy = infer_strategy_from_labels(&labels);
        assert!(strategy.is_none());
    }

    #[test]
    fn test_infer_first_matching_label_wins() {
        let labels = vec![
            "framework=patternfly".to_string(),
            "change-type=prop-removal".to_string(),
            "change-type=dom-structure".to_string(),
        ];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::RemoveAttribute)));
    }

    // -- consolidate_family_requests tests --

    fn make_llm_request(rule_id: &str, file: &str, line: u32, labels: Vec<&str>) -> LlmFixRequest {
        LlmFixRequest {
            rule_id: rule_id.to_string(),
            file_uri: format!("file://{}", file),
            file_path: PathBuf::from(file),
            line,
            message: format!("Rule {} triggered", rule_id),
            code_snip: Some(format!("// line {}", line)),
            source: None,
            labels: labels.into_iter().map(|s| s.to_string()).collect(),
            companion_test_files: Vec::new(),
        }
    }

    fn make_family_entry(target_structure: &str, new_imports: Vec<&str>) -> FixStrategyEntry {
        FixStrategyEntry {
            strategy: "FamilyMigration".to_string(),
            target_structure: Some(target_structure.to_string()),
            new_imports: new_imports.into_iter().map(|s| s.to_string()).collect(),
            import_source: Some("@patternfly/react-core".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_consolidate_single_family_request() {
        let mut requests = vec![make_llm_request(
            "mastheadbrand-signature-changed",
            "/src/viewLayout.tsx",
            16,
            vec!["family=Masthead", "change-type=prop-type-changed"],
        )];

        let mut families = BTreeMap::new();
        families.insert(
            "family:Masthead".to_string(),
            make_family_entry(
                "<Masthead>\n  <MastheadMain>\n    <MastheadBrand>\n      <MastheadLogo />\n    </MastheadBrand>\n  </MastheadMain>\n</Masthead>",
                vec!["MastheadLogo", "MastheadMain"],
            ),
        );

        consolidate_family_requests(&mut requests, &families);

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].rule_id, "family:Masthead");
        assert!(requests[0].message.contains("Masthead Family Migration"));
    }

    #[test]
    fn test_consolidate_does_not_drop_all_incidents() {
        let mut requests = vec![{
            let mut req = make_llm_request(
                "mastheadbrand-signature-changed",
                "/src/viewLayout.tsx",
                16,
                vec!["family=Masthead", "change-type=signature-changed"],
            );
            req.message =
                "Interface 'MastheadBrandProps' base class changed from anchor to div".to_string();
            req
        }];

        let mut families = BTreeMap::new();
        families.insert(
            "family:Masthead".to_string(),
            make_family_entry("<Masthead>\n  <MastheadBrand>\n    <MastheadLogo />\n  </MastheadBrand>\n</Masthead>", vec![]),
        );

        consolidate_family_requests(&mut requests, &families);

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].rule_id, "family:Masthead");
        assert!(requests[0]
            .message
            .contains("mastheadbrand-signature-changed"),);
    }

    // -- plan_import_path_change tests --

    fn make_incident(file_uri: &str, line: u32) -> Incident {
        Incident {
            file_uri: file_uri.to_string(),
            line_number: Some(line),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables: Default::default(),
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        }
    }

    #[test]
    fn test_import_path_change_single_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("single.tsx");
        std::fs::write(&file, "import { Chart } from '@patternfly/react-charts';\n").unwrap();

        let incident = make_incident("file://single.tsx", 1);
        let fix = plan_import_path_change(
            "test-rule",
            &incident,
            "@patternfly/react-charts",
            "@patternfly/react-charts/victory",
            &file,
            &mut FixReport::new(),
        );

        let fix = fix.expect("should produce a fix for single-line import");
        assert_eq!(fix.edits.len(), 1);
        assert_eq!(fix.edits[0].line, 1);
        assert_eq!(fix.edits[0].old_text, "@patternfly/react-charts");
        assert_eq!(fix.edits[0].new_text, "@patternfly/react-charts/victory");
    }

    #[test]
    fn test_import_path_change_multiline_specifier() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("multi.tsx");
        std::fs::write(
            &file,
            "\
import {
  Chart,
  ChartAxis,
  ChartBar,
} from '@patternfly/react-charts';
import { Button } from '@patternfly/react-core';
",
        )
        .unwrap();

        // Incident fires on line 2 (Chart specifier), but the from-clause
        // is on line 5.
        let incident = make_incident("file://multi.tsx", 2);
        let fix = plan_import_path_change(
            "test-rule",
            &incident,
            "@patternfly/react-charts",
            "@patternfly/react-charts/victory",
            &file,
            &mut FixReport::new(),
        );

        let fix = fix.expect("should produce a fix for multi-line import");
        assert_eq!(fix.edits.len(), 1);
        assert_eq!(
            fix.edits[0].line, 5,
            "edit should target the from-clause line"
        );
        assert_eq!(fix.edits[0].old_text, "@patternfly/react-charts");
        assert_eq!(fix.edits[0].new_text, "@patternfly/react-charts/victory");
    }

    #[test]
    fn test_import_path_change_multiline_different_specifier() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("multi2.tsx");
        std::fs::write(
            &file,
            "\
import {
  Chart,
  ChartAxis,
  ChartBar,
} from '@patternfly/react-charts';
",
        )
        .unwrap();

        // Incident on line 4 (ChartBar specifier) should also resolve to line 5.
        let incident = make_incident("file://multi2.tsx", 4);
        let fix = plan_import_path_change(
            "test-rule",
            &incident,
            "@patternfly/react-charts",
            "@patternfly/react-charts/victory",
            &file,
            &mut FixReport::new(),
        );

        let fix = fix.expect("should produce a fix for any specifier in multi-line import");
        assert_eq!(fix.edits[0].line, 5);
    }

    #[test]
    fn test_import_path_change_already_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("migrated.tsx");
        std::fs::write(
            &file,
            "import { Chart } from '@patternfly/react-charts/victory';\n",
        )
        .unwrap();

        let incident = make_incident("file://migrated.tsx", 1);
        let fix = plan_import_path_change(
            "test-rule",
            &incident,
            "@patternfly/react-charts",
            "@patternfly/react-charts/victory",
            &file,
            &mut FixReport::new(),
        );

        assert!(fix.is_none(), "should skip already-migrated imports");
    }

    #[test]
    fn test_import_path_change_multiline_already_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("migrated_multi.tsx");
        std::fs::write(
            &file,
            "\
import {
  Chart,
  ChartAxis,
} from '@patternfly/react-charts/victory';
",
        )
        .unwrap();

        let incident = make_incident("file://migrated_multi.tsx", 2);
        let fix = plan_import_path_change(
            "test-rule",
            &incident,
            "@patternfly/react-charts",
            "@patternfly/react-charts/victory",
            &file,
            &mut FixReport::new(),
        );

        assert!(
            fix.is_none(),
            "should skip already-migrated multi-line imports"
        );
    }

    #[test]
    fn test_import_path_change_non_import_context() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("jsx.tsx");
        std::fs::write(
            &file,
            "\
import { Chart } from '@patternfly/react-charts';

function App() {
  return <Chart />;
}
",
        )
        .unwrap();

        // Incident on line 4 (JSX usage), no import block above within scope.
        let incident = make_incident("file://jsx.tsx", 4);
        let fix = plan_import_path_change(
            "test-rule",
            &incident,
            "@patternfly/react-charts",
            "@patternfly/react-charts/victory",
            &file,
            &mut FixReport::new(),
        );

        // The backward scan WILL find `import` on line 1 and `{` on line 3
        // (the function body). The forward scan from line 4 won't find a
        // `from` clause with the old path, so it should return None or
        // find the wrong thing. Either way, the from-line won't contain
        // old_path so it should safely return None.
        // Actually, the backward scan finds `{` on line 3, confirming
        // "import block". Then forward scan looks for `from`, which doesn't
        // appear. So find returns None, and the `?` propagates.
        assert!(
            fix.is_none(),
            "should not produce a fix for non-import JSX usage"
        );
    }

    #[test]
    fn test_import_path_change_export_reexport() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("reexport.tsx");
        std::fs::write(
            &file,
            "\
export {
  Chart,
  ChartAxis,
} from '@patternfly/react-charts';
",
        )
        .unwrap();

        let incident = make_incident("file://reexport.tsx", 2);
        let fix = plan_import_path_change(
            "test-rule",
            &incident,
            "@patternfly/react-charts",
            "@patternfly/react-charts/victory",
            &file,
            &mut FixReport::new(),
        );

        let fix = fix.expect("should handle export re-exports");
        assert_eq!(fix.edits[0].line, 4);
        assert_eq!(fix.edits[0].old_text, "@patternfly/react-charts");
    }

    #[test]
    fn test_consolidate_preserves_fix_strategy_context() {
        let mut requests = vec![{
            let mut req = make_llm_request(
                "menutoggle-splitbuttonoptions-changed",
                "/src/ToolbarBulkSelector.tsx",
                134,
                vec!["family=MenuToggle", "change-type=prop-type-changed"],
            );
            // Simulate the enriched message with both incident context and fix strategy
            req.message = "property splitButtonOptions was replaced by splitButtonItems\n\n\
                Incident context:\n  componentName: MenuToggle\n  propName: splitButtonOptions\n\n\
                Fix strategy:\nStrategy: PropTypeChange\nComponent: MenuToggle\n\
                Prop: splitButtonOptions\nFrom: property: splitButtonOptions: SplitButtonOptions\n\
                To: property: splitButtonItems: ReactNode[]"
                .to_string();
            req
        }];

        let mut families = BTreeMap::new();
        families.insert(
            "family:MenuToggle".to_string(),
            make_family_entry(
                "<MenuToggle splitButtonItems={...} />",
                vec!["MenuToggleAction"],
            ),
        );

        consolidate_family_requests(&mut requests, &families);

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].rule_id, "family:MenuToggle");
        // Verify incident context variables are preserved
        assert!(
            requests[0].message.contains("componentName: MenuToggle"),
            "incident context variables should be preserved"
        );
        // Verify fix strategy context is preserved (this was previously dropped)
        assert!(
            requests[0].message.contains("Strategy: PropTypeChange"),
            "fix strategy type should be preserved"
        );
        assert!(
            requests[0]
                .message
                .contains("From: property: splitButtonOptions: SplitButtonOptions"),
            "fix strategy 'from' should be preserved"
        );
        assert!(
            requests[0]
                .message
                .contains("To: property: splitButtonItems: ReactNode[]"),
            "fix strategy 'to' should be preserved"
        );
    }

    // -- deduplicate_edits tests --

    /// Helper to build a PlannedFix with a single TextEdit.
    fn make_edit(line: u32, old: &str, new: &str, replace_all: bool) -> PlannedFix {
        PlannedFix {
            edits: vec![TextEdit {
                line,
                old_text: old.into(),
                new_text: new.into(),
                rule_id: "test".into(),
                description: "test".into(),
                replace_all,
            }],
            confidence: konveyor_core::fix::FixConfidence::Exact,
            source: konveyor_core::fix::FixSource::Pattern,
            rule_id: "test".into(),
            file_uri: "file:///test.tsx".into(),
            line,
            description: "test".into(),
        }
    }

    #[test]
    fn test_dedup_replace_all_substring_edits_both_survive() {
        // Bug scenario: two CSS class renames on the same line, one is a
        // substring of the other. Both have replace_all=true.
        // className="pf-v5-c-form__label pf-v5-c-form__label-text"
        //
        // Before fix: the shorter edit ("pf-v5-c-form__label") was subsumed
        // because it's a substring of the longer ("pf-v5-c-form__label-text").
        // After fix: both survive because replace_all edits applied
        // longest-first are safe.
        let mut plan = FixPlan::default();
        let file = PathBuf::from("/test.tsx");
        plan.files.insert(
            file.clone(),
            vec![
                make_edit(10, "pf-v5-c-form__label-text", "pf-v6-c-form__label-text", true),
                make_edit(10, "pf-v5-c-form__label", "pf-v6-c-form__label", true),
            ],
        );

        deduplicate_edits(&mut plan);

        let edits: Vec<&TextEdit> = plan.files[&file]
            .iter()
            .flat_map(|f| &f.edits)
            .collect();
        assert_eq!(
            edits.len(),
            2,
            "Both replace_all edits should survive dedup, got {}",
            edits.len()
        );
        assert_eq!(plan.edits_subsumed, 0);
    }

    #[test]
    fn test_dedup_non_replace_all_substring_still_subsumed() {
        // When replace_all=false, the old behavior should be preserved:
        // a shorter old_text that's a substring of a longer kept edit is
        // subsumed.
        let mut plan = FixPlan::default();
        let file = PathBuf::from("/test.tsx");
        plan.files.insert(
            file.clone(),
            vec![
                make_edit(10, "FooBar", "BazBar", false),
                make_edit(10, "Foo", "Baz", false),
            ],
        );

        deduplicate_edits(&mut plan);

        let edits: Vec<&TextEdit> = plan.files[&file]
            .iter()
            .flat_map(|f| &f.edits)
            .collect();
        assert_eq!(
            edits.len(),
            1,
            "Non-replace_all shorter edit should be subsumed, got {}",
            edits.len()
        );
        assert_eq!(edits[0].old_text, "FooBar");
        assert_eq!(plan.edits_subsumed, 1);
    }

    #[test]
    fn test_dedup_mixed_replace_all_substring_subsumed() {
        // When one edit is replace_all and the other is not, the shorter
        // edit is still subsumed (conservative behavior).
        let mut plan = FixPlan::default();
        let file = PathBuf::from("/test.tsx");
        plan.files.insert(
            file.clone(),
            vec![
                make_edit(10, "pf-v5-c-form__label-text", "pf-v6-c-form__label-text", true),
                make_edit(10, "pf-v5-c-form__label", "pf-v6-c-form__label", false),
            ],
        );

        deduplicate_edits(&mut plan);

        let edits: Vec<&TextEdit> = plan.files[&file]
            .iter()
            .flat_map(|f| &f.edits)
            .collect();
        assert_eq!(
            edits.len(),
            1,
            "Mixed replace_all: shorter non-replace_all should be subsumed"
        );
        assert_eq!(edits[0].old_text, "pf-v5-c-form__label-text");
    }

    #[test]
    fn test_dedup_triple_nesting_replace_all() {
        // Three CSS classes with shared prefix, all replace_all=true.
        // All three should survive.
        let mut plan = FixPlan::default();
        let file = PathBuf::from("/test.tsx");
        plan.files.insert(
            file.clone(),
            vec![
                make_edit(10, "pf-v5-c-form__label-required", "pf-v6-c-form__label-required", true),
                make_edit(10, "pf-v5-c-form__label-text", "pf-v6-c-form__label-text", true),
                make_edit(10, "pf-v5-c-form__label", "pf-v6-c-form__label", true),
            ],
        );

        deduplicate_edits(&mut plan);

        let edits: Vec<&TextEdit> = plan.files[&file]
            .iter()
            .flat_map(|f| &f.edits)
            .collect();
        assert_eq!(
            edits.len(),
            3,
            "All three replace_all edits should survive, got {}",
            edits.len()
        );
        assert_eq!(plan.edits_subsumed, 0);
    }

    #[test]
    fn test_dedup_exact_duplicate_still_removed() {
        // Exact duplicates (same old+new) should still be removed,
        // even with replace_all=true.
        let mut plan = FixPlan::default();
        let file = PathBuf::from("/test.tsx");
        plan.files.insert(
            file.clone(),
            vec![
                make_edit(10, "pf-v5-c-form__label", "pf-v6-c-form__label", true),
                make_edit(10, "pf-v5-c-form__label", "pf-v6-c-form__label", true),
            ],
        );

        deduplicate_edits(&mut plan);

        let edits: Vec<&TextEdit> = plan.files[&file]
            .iter()
            .flat_map(|f| &f.edits)
            .collect();
        assert_eq!(edits.len(), 1, "Exact duplicate should be removed");
        assert_eq!(plan.edits_subsumed, 1);
    }
}
