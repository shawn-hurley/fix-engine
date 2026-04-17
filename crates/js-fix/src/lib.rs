//! JS/TS/JSX/TSX language-specific fix operations.
//!
//! Implements [`LanguageFixProvider`] for the JavaScript/TypeScript ecosystem:
//! - Skips `node_modules/` paths
//! - Deduplicates ES import specifiers after renames
//! - Removes JSX attributes (props) using syntax-aware regex
//! - Extracts matched text from JSX/React incident variables
//! - Manages `package.json` dependencies
//! - Resolves ecosystem dependency versions via npm registry
//! - Resolves transitive dependency conflicts from lockfiles

mod lockfile;

use fix_engine::language::LanguageFixProvider;
use fix_engine_core::*;
use konveyor_core::incident::Incident;
use std::path::{Path, PathBuf};

/// Language fix provider for JavaScript/TypeScript/JSX/TSX files.
pub struct JsFixProvider;

impl JsFixProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsFixProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageFixProvider for JsFixProvider {
    fn should_skip_path(&self, path: &Path) -> bool {
        // Skip node_modules — these are updated via package.json
        // version bumps, not by patching source directly.
        // Note: src/vendor/ is NOT skipped — vendored source code
        // (e.g., forked libraries) is compiled as part of the project
        // and needs migration alongside the rest of the codebase.
        path.components().any(|c| c.as_os_str() == "node_modules")
    }

    fn post_process_lines(&self, lines: &mut [String]) {
        dedup_import_specifiers(lines);
    }

    fn plan_remove_attribute(
        &self,
        rule_id: &str,
        incident: &Incident,
        file_path: &Path,
    ) -> Option<PlannedFix> {
        plan_remove_prop(rule_id, incident, file_path)
    }

    fn plan_ensure_dependency(
        &self,
        rule_id: &str,
        incident: &Incident,
        package: &str,
        new_version: &str,
        file_path: &Path,
    ) -> Vec<PlannedFix> {
        plan_ensure_npm_dependency(rule_id, incident, package, new_version, file_path)
    }

    fn get_matched_text(&self, incident: &Incident) -> String {
        get_matched_text_from_incident(incident)
    }

    fn get_matched_text_for_rename(
        &self,
        incident: &Incident,
        mappings: &[RenameMapping],
    ) -> String {
        get_matched_text_for_rename_from_incident(incident, mappings)
    }

    fn is_whole_file_rename(&self, incident: &Incident) -> bool {
        // Component/import renames (detected via importedName variable) need
        // whole-file scanning since JSX usage of the component appears on many
        // lines beyond the import: opening tags, closing tags, type references.
        incident.variables.contains_key("importedName")
    }

    fn post_apply(
        &self,
        project_root: &Path,
        modified_files: &[std::path::PathBuf],
    ) -> anyhow::Result<()> {
        // Check if any package.json was modified — if so, run install to
        // regenerate the lockfile and node_modules.
        let any_package_json = modified_files
            .iter()
            .any(|p| p.file_name().and_then(|f| f.to_str()) == Some("package.json"));

        if !any_package_json {
            return Ok(());
        }

        tracing::info!("package.json was modified, running install to sync lockfile");

        if project_root.join("yarn.lock").exists() {
            run_yarn_install_and_resolve_peers(project_root);
        } else if project_root.join("pnpm-lock.yaml").exists() {
            run_pnpm_install(project_root);
        } else {
            run_npm_install(project_root);
        }

        Ok(())
    }
}

// ── Post-apply install helpers ──────────────────────────────────────────

/// Run `yarn install`, parse peer dependency warnings (YN0002), and install
/// any missing peers automatically.
///
/// Yarn berry does not auto-install peer dependencies and has no config to
/// enable it. We capture its output, parse the YN0002 warning lines to
/// extract the names of missing peer packages, then run `yarn add` for them.
fn run_yarn_install_and_resolve_peers(project_root: &Path) {
    tracing::info!("Running yarn install --ignore-scripts");

    let output = std::process::Command::new("yarn")
        .args(["install", "--ignore-scripts"])
        .current_dir(project_root)
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("yarn install could not be executed: {}", e);
            return;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("yarn install failed: {}", stderr.trim());
    }

    // Yarn berry writes warnings to stdout. Parse YN0002 lines for missing peers.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let missing_peers = parse_yarn_missing_peer_deps(&stdout);

    if missing_peers.is_empty() {
        tracing::info!("yarn install completed, no missing peer dependencies");
        return;
    }

    tracing::info!(
        count = missing_peers.len(),
        peers = ?missing_peers,
        "Installing missing peer dependencies detected from yarn warnings"
    );

    let add_result = std::process::Command::new("yarn")
        .arg("add")
        .args(&missing_peers)
        .arg("--ignore-scripts")
        .current_dir(project_root)
        .output();

    match add_result {
        Ok(o) if o.status.success() => {
            tracing::info!("Successfully installed missing peer dependencies");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("yarn add for peer dependencies failed: {}", stderr.trim());
        }
        Err(e) => {
            tracing::warn!("yarn add could not be executed: {}", e);
        }
    }
}

/// Parse yarn berry output for YN0002 (missing peer dependency) warnings.
///
/// Yarn berry emits lines like:
/// ```text
/// ➤ YN0002: @patternfly/react-charts@npm:8.4.1 doesn't provide victory (p1a2b3), requested by ...
/// ```
///
/// The output contains ANSI escape codes which are stripped before matching.
/// Returns a deduplicated list of missing peer package names.
fn parse_yarn_missing_peer_deps(output: &str) -> Vec<String> {
    // Strip ANSI escape codes: ESC[ followed by parameters and a letter
    let ansi_re = regex::Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").expect("valid regex");
    let stripped = ansi_re.replace_all(output, "");

    // Match YN0002 lines:
    //   YN0002: <locator> doesn't provide <peer_name> (<hash>), requested by ...
    // The peer name can be scoped (e.g., @scope/pkg) or unscoped.
    let peer_re =
        regex::Regex::new(r"YN0002: .+ doesn't provide (@?[^\s(]+) \(").expect("valid regex");

    let mut seen = std::collections::HashSet::new();
    peer_re
        .captures_iter(&stripped)
        .filter_map(|cap| {
            let name = cap[1].to_string();
            seen.insert(name.clone()).then_some(name)
        })
        .collect()
}

/// Run `pnpm install` with auto-install-peers enabled via env var.
///
/// pnpm supports `auto-install-peers` (default true since v8) but we set
/// the env var explicitly to ensure it works on older pnpm versions too.
fn run_pnpm_install(project_root: &Path) {
    tracing::info!("Running pnpm install --ignore-scripts (with auto-install-peers)");

    let output = std::process::Command::new("pnpm")
        .args(["install", "--ignore-scripts"])
        .env("npm_config_auto_install_peers", "true")
        .current_dir(project_root)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            tracing::info!("pnpm install completed successfully");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("pnpm install failed: {}", stderr.trim());
        }
        Err(e) => {
            tracing::warn!("pnpm install could not be executed: {}", e);
        }
    }
}

/// Run `npm install`. npm v7+ auto-installs peer dependencies by default.
fn run_npm_install(project_root: &Path) {
    tracing::info!("Running npm install --ignore-scripts --no-audit --no-fund");

    let output = std::process::Command::new("npm")
        .args(["install", "--ignore-scripts", "--no-audit", "--no-fund"])
        .current_dir(project_root)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            tracing::info!("npm install completed successfully");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("npm install failed: {}", stderr.trim());
        }
        Err(e) => {
            tracing::warn!("npm install could not be executed: {}", e);
        }
    }
}

// -- JSX prop removal --

fn plan_remove_prop(rule_id: &str, incident: &Incident, file_path: &Path) -> Option<PlannedFix> {
    let line = incident.line_number?;
    let prop_name = incident
        .variables
        .get("propName")
        .and_then(|v| v.as_str())?;

    // Read the actual file line to construct a precise removal edit.
    let source = std::fs::read_to_string(file_path).ok()?;
    let all_lines: Vec<&str> = source.lines().collect();
    let line_idx = (line as usize).saturating_sub(1);
    let file_line = all_lines.get(line_idx)?;
    let trimmed = file_line.trim();

    // If the entire line is just the prop (common in formatted JSX), remove it.
    if trimmed.starts_with(prop_name) {
        let depth = bracket_depth(file_line);
        if depth == 0 {
            // Single-line prop -- safe to remove just this line
            Some(PlannedFix {
                edits: vec![TextEdit {
                    line,
                    old_text: file_line.to_string(),
                    new_text: String::new(),
                    rule_id: rule_id.to_string(),
                    description: format!("Remove prop '{}' (entire line)", prop_name),
                    replace_all: false,
                }],
                confidence: FixConfidence::High,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.file_uri.clone(),
                line,
                description: format!("Remove prop '{}'", prop_name),
            })
        } else {
            // Multi-line prop value -- scan forward to find where brackets balance.
            let mut cumulative_depth = depth;
            let mut end_idx = line_idx;
            for (i, subsequent_line) in all_lines.iter().enumerate().skip(line_idx + 1) {
                cumulative_depth += bracket_depth(subsequent_line);
                end_idx = i;
                if cumulative_depth <= 0 {
                    break;
                }
            }

            if cumulative_depth > 0 {
                return Some(PlannedFix {
                    edits: vec![],
                    confidence: FixConfidence::Low,
                    source: FixSource::Pattern,
                    rule_id: rule_id.to_string(),
                    file_uri: incident.file_uri.clone(),
                    line,
                    description: format!(
                        "Remove prop '{}' (unbalanced brackets, manual)",
                        prop_name
                    ),
                });
            }

            // Remove all lines from prop start through closing bracket
            let mut edits = Vec::new();
            for i in line_idx..=end_idx {
                if let Some(l) = all_lines.get(i) {
                    edits.push(TextEdit {
                        line: (i + 1) as u32,
                        old_text: l.to_string(),
                        new_text: String::new(),
                        rule_id: rule_id.to_string(),
                        description: format!(
                            "Remove prop '{}' (line {} of multi-line)",
                            prop_name,
                            i - line_idx + 1
                        ),
                        replace_all: false,
                    });
                }
            }

            Some(PlannedFix {
                edits,
                confidence: FixConfidence::High,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.file_uri.clone(),
                line,
                description: format!(
                    "Remove prop '{}' ({} lines)",
                    prop_name,
                    end_idx - line_idx + 1
                ),
            })
        }
    } else {
        // Prop is inline with other content -- try to remove just the prop fragment.
        let prop_re = regex::Regex::new(&format!(
            r#"\s+{prop_name}(?:=\{{[^}}]*\}}|="[^"]*"|='[^']*'|=\{{.*?\}})?"#
        ))
        .ok()?;

        if let Some(m) = prop_re.find(file_line) {
            if bracket_depth(m.as_str()) != 0 {
                return Some(PlannedFix {
                    edits: vec![],
                    confidence: FixConfidence::Low,
                    source: FixSource::Pattern,
                    rule_id: rule_id.to_string(),
                    file_uri: incident.file_uri.clone(),
                    line,
                    description: format!("Remove prop '{}' (multi-line inline, manual)", prop_name),
                });
            }

            Some(PlannedFix {
                edits: vec![TextEdit {
                    line,
                    old_text: m.as_str().to_string(),
                    new_text: String::new(),
                    rule_id: rule_id.to_string(),
                    description: format!("Remove prop '{}'", prop_name),
                    replace_all: false,
                }],
                confidence: FixConfidence::High,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.file_uri.clone(),
                line,
                description: format!("Remove prop '{}'", prop_name),
            })
        } else {
            Some(PlannedFix {
                edits: vec![],
                confidence: FixConfidence::Low,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.file_uri.clone(),
                line,
                description: format!("Remove prop '{}' (manual)", prop_name),
            })
        }
    }
}

// -- Import deduplication --

/// Deduplicate import specifiers on lines that look like ES import statements.
fn dedup_import_specifiers(lines: &mut [String]) {
    let import_re = regex::Regex::new(r"^(\s*import\s+\{)([^}]+)(\}\s*from\s+.*)$").unwrap();

    for line in lines.iter_mut() {
        if let Some(caps) = import_re.captures(line) {
            let prefix = caps.get(1).unwrap().as_str();
            let specifiers_str = caps.get(2).unwrap().as_str();
            let suffix = caps.get(3).unwrap().as_str();

            let specifiers: Vec<&str> = specifiers_str
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();

            let mut seen = std::collections::HashSet::new();
            let deduped: Vec<&str> = specifiers
                .into_iter()
                .filter(|s| seen.insert(s.to_string()))
                .collect();

            let new_specifiers = format!(" {} ", deduped.join(", "));
            let new_line = format!("{}{}{}", prefix, new_specifiers, suffix);

            if new_line != *line {
                *line = new_line;
            }
        }
    }
}

// -- Bracket depth --

/// Count net bracket/brace depth change for a line.
fn bracket_depth(line: &str) -> i32 {
    let mut depth: i32 = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_backtick = false;
    let mut prev = '\0';
    for ch in line.chars() {
        match ch {
            '\'' if !in_double_quote && !in_backtick && prev != '\\' => {
                in_single_quote = !in_single_quote
            }
            '"' if !in_single_quote && !in_backtick && prev != '\\' => {
                in_double_quote = !in_double_quote
            }
            '`' if !in_single_quote && !in_double_quote && prev != '\\' => {
                in_backtick = !in_backtick
            }
            '(' | '{' | '[' if !in_single_quote && !in_double_quote && !in_backtick => depth += 1,
            ')' | '}' | ']' if !in_single_quote && !in_double_quote && !in_backtick => depth -= 1,
            _ => {}
        }
        prev = ch;
    }
    depth
}

// -- Incident variable extraction --

/// Extract the matched text from incident variables.
fn get_matched_text_from_incident(incident: &Incident) -> String {
    for key in &[
        "propName",
        "componentName",
        "importedName",
        "className",
        "variableName",
    ] {
        if let Some(serde_json::Value::String(s)) = incident.variables.get(*key) {
            return s.clone();
        }
    }
    String::new()
}

/// Get the matched text, considering both prop names and prop values.
fn get_matched_text_for_rename_from_incident(
    incident: &Incident,
    mappings: &[RenameMapping],
) -> String {
    let prop_name = get_matched_text_from_incident(incident);

    if mappings.iter().any(|m| m.old == prop_name) {
        return prop_name;
    }

    if let Some(serde_json::Value::String(val)) = incident.variables.get("propValue") {
        if mappings.iter().any(|m| m.old == val.as_str()) {
            return val.clone();
        }
    }

    if let Some(serde_json::Value::Array(vals)) = incident.variables.get("propObjectValues") {
        for v in vals {
            if let serde_json::Value::String(s) = v {
                if mappings.iter().any(|m| m.old == s.as_str()) {
                    return s.clone();
                }
            }
        }
    }

    prop_name
}

// -- npm dependency management (package.json) --

/// Walk up the directory tree from `path` to find the nearest `package.json`.
fn find_nearest_package_json(path: &Path) -> Option<PathBuf> {
    let mut dir = if path.is_file() { path.parent()? } else { path };
    loop {
        let candidate = dir.join("package.json");
        if candidate.exists() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

/// Ensure a dependency exists at the correct version in `package.json`.
///
/// Three paths:
///
/// 1. **Lockfile incident** (URI points to a lockfile): The incident fired on a
///    transitive copy of the package. Parse the lockfile to find which direct
///    deps in `package.json` pull it in, resolve their latest compatible
///    versions from npm, and plan updates for those parent packages.
///
/// 2. **Dependent incident** (has `isDependentOf` variable): Legacy path for
///    transitive conflicts detected by the lockfile scanner in the provider.
///    Resolves the actual package from `dependencyName` via npm.
///
/// 3. **Direct incident** (URI points to `package.json` or source file): Update
///    or insert the package in `package.json` with the given version.
fn plan_ensure_npm_dependency(
    rule_id: &str,
    incident: &Incident,
    package: &str,
    new_version: &str,
    file_path: &Path,
) -> Vec<PlannedFix> {
    // ── Path 1: Lockfile incident ────────────────────────────────────
    //
    // When the incident URI points to a lockfile (yarn.lock, package-lock.json,
    // pnpm-lock.yaml), the rule fired on a transitive copy of `package` (e.g.,
    // a nested @patternfly/react-core@5.x pulled in by react-topology).
    //
    // Instead of redundantly updating the target package (the direct incident
    // handles that), we find which direct deps bring in the transitive copy
    // and update those parent packages to versions compatible with the new
    // major version of the target.
    if lockfile::is_lockfile(file_path) {
        tracing::info!(
            package = %package,
            lockfile = %file_path.display(),
            "Lockfile incident: resolving parent packages for transitive dependency"
        );

        // Find the sibling package.json
        let pkg_json = match find_nearest_package_json(file_path) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    lockfile = %file_path.display(),
                    "No package.json found near lockfile; skipping"
                );
                return Vec::new();
            }
        };

        // Get the set of direct dep names from package.json
        let direct_deps = lockfile::parse_direct_dep_names(&pkg_json);

        // Find which lockfile entries transitively depend on the target package.
        // This walks up the dependency chain: if A → B → C and C is the target,
        // both A and B are returned as ancestors.
        let all_parents = lockfile::find_transitive_ancestor_packages(file_path, package);

        // Filter to only direct deps (we can only update what's in package.json)
        let actionable_parents: Vec<&String> = all_parents
            .iter()
            .filter(|name| direct_deps.contains(name.as_str()))
            .collect();

        if actionable_parents.is_empty() {
            tracing::debug!(
                package = %package,
                "No direct-dep parents found for transitive lockfile dep; skipping"
            );
            return Vec::new();
        }

        let target_major = extract_major(new_version);
        let mut fixes = Vec::new();

        for parent in &actionable_parents {
            tracing::info!(
                parent = %parent,
                compatible_with = %package,
                target_major = target_major,
                "Resolving npm-compatible version for lockfile parent"
            );

            let resolved = resolve_npm_compatible_version(parent, package, target_major);

            match resolved {
                Some(ref ver) => {
                    tracing::info!(
                        parent = %parent,
                        resolved_version = %ver,
                        "Resolved npm-compatible version for lockfile parent"
                    );
                    if let Some(fix) =
                        plan_ensure_npm_dependency_inner(rule_id, &pkg_json, parent, ver)
                    {
                        fixes.push(fix);
                    }
                }
                None => {
                    tracing::warn!(
                        parent = %parent,
                        compatible_with = %package,
                        "Could not resolve compatible version from npm; skipping parent"
                    );
                }
            }
        }

        tracing::info!(
            package = %package,
            parents = actionable_parents.len(),
            fixes = fixes.len(),
            "Lockfile incident resolved"
        );

        return fixes;
    }

    // ── Path 2: Dependent incident (isDependentOf variable) ──────────
    //
    // Legacy path for transitive conflicts with explicit variables.
    if let Some(serde_json::Value::String(depends_on)) = incident.variables.get("isDependentOf") {
        let actual_package = match incident
            .variables
            .get("dependencyName")
            .and_then(|v| v.as_str())
        {
            Some(p) => p,
            None => return Vec::new(),
        };

        let target_major = extract_major(new_version);

        tracing::info!(
            dependent = %actual_package,
            depends_on = %depends_on,
            target_major = target_major,
            "Resolving compatible version from npm for dependent package"
        );

        let resolved = resolve_npm_compatible_version(actual_package, depends_on, target_major);

        return match resolved {
            Some(ref ver) => {
                tracing::info!(
                    package = %actual_package,
                    resolved_version = %ver,
                    "Resolved npm-compatible version for dependent"
                );
                plan_ensure_npm_dependency_inner(rule_id, file_path, actual_package, ver)
                    .into_iter()
                    .collect()
            }
            None => {
                tracing::warn!(
                    package = %actual_package,
                    depends_on = %depends_on,
                    "Could not resolve compatible version from npm; skipping"
                );
                Vec::new()
            }
        };
    }

    // ── Path 3: Direct incident ──────────────────────────────────────
    plan_ensure_npm_dependency_inner(rule_id, file_path, package, new_version)
        .into_iter()
        .collect()
}

/// Inner implementation: update or insert a dependency in package.json.
fn plan_ensure_npm_dependency_inner(
    rule_id: &str,
    file_path: &Path,
    package: &str,
    new_version: &str,
) -> Option<PlannedFix> {
    // Resolve the target package.json
    let pkg_json = if file_path.file_name().is_some_and(|f| f == "package.json") {
        file_path.to_path_buf()
    } else {
        find_nearest_package_json(file_path)?
    };

    let source = std::fs::read_to_string(&pkg_json).ok()?;
    let pkg_json_uri = format!("file://{}", pkg_json.display());

    // --- Identify top-level dependency blocks ---
    // We need to distinguish top-level "dependencies" / "devDependencies" from
    // nested ones (e.g., "consolePlugin.dependencies"). Top-level blocks start
    // at JSON brace depth 1 (inside the root object).
    let lines: Vec<&str> = source.lines().collect();
    let top_level_dep_ranges = find_top_level_dep_blocks(&lines);

    // --- Try update: find the package in a top-level dep block and replace its version ---
    let package_quoted = format!("\"{}\"", package);
    let version_re = regex::Regex::new(r#"("[\^~><=]*[0-9][^"]*")"#).ok()?;

    for (idx, file_line) in source.lines().enumerate() {
        if !file_line.contains(&package_quoted) {
            continue;
        }
        // Only match if this line is inside a top-level dep block
        if !top_level_dep_ranges
            .iter()
            .any(|r| idx >= r.start && idx < r.end)
        {
            continue;
        }
        if let Some(m) = version_re.find(file_line) {
            let line = (idx + 1) as u32;
            let old_version = m.as_str();
            let new_ver_quoted = format!("\"{}\"", new_version);

            return Some(PlannedFix {
                edits: vec![TextEdit {
                    line,
                    old_text: old_version.to_string(),
                    new_text: new_ver_quoted.clone(),
                    rule_id: rule_id.to_string(),
                    description: format!(
                        "Update {} from {} to {}",
                        package, old_version, new_ver_quoted
                    ),
                    replace_all: false,
                }],
                confidence: FixConfidence::Exact,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: pkg_json_uri,
                line,
                description: format!("Update {} to {}", package, new_version),
            });
        }
    }

    // --- Insert: package not found, add it to a top-level dep block ---
    // Prefer "devDependencies" if it exists (most PF consumer deps live there),
    // fall back to "dependencies".
    let target_block = top_level_dep_ranges
        .iter()
        .find(|r| r.name == "devDependencies")
        .or_else(|| {
            top_level_dep_ranges
                .iter()
                .find(|r| r.name == "dependencies")
        });

    let target_block = target_block?;
    let mut last_entry_line: Option<usize> = None;
    let mut closing_brace_line: Option<usize> = None;

    for idx in target_block.start..target_block.end {
        let trimmed = lines[idx].trim();
        if trimmed == "}" || trimmed == "}," {
            closing_brace_line = Some(idx);
            break;
        }
        if !trimmed.is_empty()
            && !trimmed.starts_with("\"dependencies\"")
            && !trimmed.starts_with("\"devDependencies\"")
            && trimmed != "{"
        {
            last_entry_line = Some(idx);
        }
    }

    let closing_idx = closing_brace_line?;
    let closing_line_num = (closing_idx + 1) as u32;

    let entry_indent = if let Some(last_idx) = last_entry_line {
        let last = lines[last_idx];
        let indent_len = last.len() - last.trim_start().len();
        &last[..indent_len]
    } else {
        "    "
    };

    let mut edits = Vec::new();

    if let Some(last_idx) = last_entry_line {
        let last = lines[last_idx];
        if !last.trim_end().ends_with(',') {
            let last_line_num = (last_idx + 1) as u32;
            let trimmed_last = last.trim_end().to_string();
            edits.push(TextEdit {
                line: last_line_num,
                old_text: trimmed_last.clone(),
                new_text: format!("{},", trimmed_last),
                rule_id: rule_id.to_string(),
                description: format!("Add trailing comma before new dependency {}", package),
                replace_all: false,
            });
        }
    }

    let closing_line_text = lines[closing_idx].to_string();
    let new_entry = format!(
        "{}\"{}\": \"{}\"\n{}",
        entry_indent, package, new_version, closing_line_text
    );
    edits.push(TextEdit {
        line: closing_line_num,
        old_text: closing_line_text,
        new_text: new_entry,
        rule_id: rule_id.to_string(),
        description: format!("Add {} {} to dependencies", package, new_version),
        replace_all: false,
    });

    Some(PlannedFix {
        edits,
        confidence: FixConfidence::Exact,
        source: FixSource::Pattern,
        rule_id: rule_id.to_string(),
        file_uri: pkg_json_uri,
        line: closing_line_num,
        description: format!("Add {} {} to dependencies", package, new_version),
    })
}

// -- Top-level dependency block detection --

/// A range of lines in package.json belonging to a top-level dependency block.
struct DepBlockRange {
    /// "dependencies" or "devDependencies"
    name: &'static str,
    /// Start line index (inclusive, the key line)
    start: usize,
    /// End line index (exclusive, after the closing brace)
    end: usize,
}

/// Find top-level "dependencies" and "devDependencies" blocks in package.json.
///
/// Top-level means at JSON depth 1 (direct children of the root object).
/// Nested blocks like "consolePlugin.dependencies" are at depth >= 2 and
/// are excluded.
fn find_top_level_dep_blocks(lines: &[&str]) -> Vec<DepBlockRange> {
    let mut results = Vec::new();
    let mut root_depth: i32 = 0;

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Track root-level brace depth (outside any dep block scan)
        for ch in trimmed.chars() {
            match ch {
                '{' => root_depth += 1,
                '}' => root_depth -= 1,
                _ => {}
            }
        }

        // Only match dep block keys at depth 1 (just entered the root object)
        // After processing braces above, a line like `"dependencies": {` will
        // have bumped root_depth to 2. So we check for depth == 2 for a
        // combined key+brace line, or depth == 1 for key-only lines.
        let is_dep_key = (root_depth == 2 || root_depth == 1)
            && (trimmed.starts_with("\"dependencies\"")
                || trimmed.starts_with("\"devDependencies\""));

        if !is_dep_key {
            i += 1;
            continue;
        }

        let name = if trimmed.starts_with("\"devDependencies\"") {
            "devDependencies"
        } else {
            "dependencies"
        };

        let start = i;
        // Find the matching closing brace for this block
        let mut block_depth: i32 = 0;
        for ch in trimmed.chars() {
            match ch {
                '{' => block_depth += 1,
                '}' => block_depth -= 1,
                _ => {}
            }
        }

        i += 1;
        while i < lines.len() && block_depth > 0 {
            let t = lines[i].trim();
            for ch in t.chars() {
                match ch {
                    '{' => {
                        block_depth += 1;
                        root_depth += 1;
                    }
                    '}' => {
                        block_depth -= 1;
                        root_depth -= 1;
                    }
                    _ => {}
                }
            }
            i += 1;
        }

        results.push(DepBlockRange {
            name,
            start,
            end: i,
        });
    }

    results
}

// -- npm registry resolution --

/// Extract the major version number from a version string.
/// `"^6.4.1"` → 6, `"6.4.1"` → 6, `"~5.0.0"` → 5
fn extract_major(version: &str) -> u64 {
    let stripped = version
        .trim()
        .trim_start_matches('^')
        .trim_start_matches('~')
        .trim_start_matches(">=")
        .trim_start_matches("<=")
        .trim_start_matches('>')
        .trim_start_matches('<')
        .trim_start_matches('=');
    stripped
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Query the npm registry to find the latest stable version of `package`
/// whose dependency on `compatible_with` uses major version `target_major`.
///
/// For example, for `resolve_npm_compatible_version("@patternfly/react-topology",
/// "@patternfly/react-core", 6)`, this finds the latest version of
/// `react-topology` that depends on `@patternfly/react-core@^6.x`.
///
/// Returns the version as `"^X.Y.Z"` or `None` if no compatible version
/// is found or the registry query fails.
fn resolve_npm_compatible_version(
    package: &str,
    compatible_with: &str,
    target_major: u64,
) -> Option<String> {
    let url = format!("https://registry.npmjs.org/{}", package);

    let mut response = match ureq::get(&url).call() {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(
                package = %package,
                error = %e,
                "npm registry query failed"
            );
            return None;
        }
    };

    let body: serde_json::Value = match response.body_mut().read_json() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                package = %package,
                error = %e,
                "Failed to parse npm registry response"
            );
            return None;
        }
    };

    let versions = body.get("versions")?.as_object()?;

    // Find all stable versions whose dependency on `compatible_with`
    // has a major version >= target_major
    let mut candidates: Vec<(u64, u64, u64, &str)> = Vec::new();

    for (ver_str, ver_data) in versions {
        // Skip prereleases
        if ver_str.contains("alpha")
            || ver_str.contains("prerelease")
            || ver_str.contains("rc")
            || ver_str.contains("beta")
        {
            continue;
        }

        // Check if this version is compatible with the target major of
        // compatible_with. A version is compatible if:
        // 1. It declares compatible_with at target_major+ (explicit compat), OR
        // 2. It doesn't declare compatible_with at all (the dep was dropped,
        //    meaning no version constraint — implicitly compatible with any version)
        //
        // A version is INCOMPATIBLE only if it explicitly constrains
        // compatible_with to a major version below target_major.
        let dep_constraint = ["dependencies", "peerDependencies"]
            .iter()
            .find_map(|section| {
                ver_data
                    .get(*section)
                    .and_then(|deps| deps.get(compatible_with))
                    .and_then(|c| c.as_str())
            });

        let is_compatible = match dep_constraint {
            Some(c) => extract_major(c) >= target_major,
            None => true, // dep dropped — no constraint, implicitly compatible
        };

        if !is_compatible {
            continue;
        }

        // Parse version for sorting
        if let Some(parsed) = parse_semver_tuple(ver_str) {
            candidates.push((parsed.0, parsed.1, parsed.2, ver_str.as_str()));
        }
    }

    // Sort and take the latest
    candidates.sort();
    let latest = candidates.last()?;

    Some(format!("^{}", latest.3))
}

/// Parse a semver string into (major, minor, patch) tuple.
fn parse_semver_tuple(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim();
    let version_part = s.split('-').next().unwrap_or(s);
    let parts: Vec<&str> = version_part.split('.').collect();
    let major = parts.first()?.parse().ok()?;
    let minor = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Create a test Incident with just the fields the fix provider cares about.
    fn make_test_incident(
        uri: &str,
        line: u32,
        variables: BTreeMap<String, serde_json::Value>,
    ) -> Incident {
        Incident {
            file_uri: uri.to_string(),
            line_number: Some(line),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables,
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        }
    }

    // -- should_skip_path tests --

    #[test]
    fn test_skip_node_modules() {
        let provider = JsFixProvider::new();
        assert!(provider.should_skip_path(Path::new(
            "/project/node_modules/@patternfly/react-core/index.js"
        )));
    }

    #[test]
    fn test_skip_nested_node_modules() {
        let provider = JsFixProvider::new();
        assert!(
            provider.should_skip_path(Path::new("/project/packages/app/node_modules/foo/bar.ts"))
        );
    }

    #[test]
    fn test_does_not_skip_src() {
        let provider = JsFixProvider::new();
        assert!(!provider.should_skip_path(Path::new("/project/src/App.tsx")));
    }

    #[test]
    fn test_does_not_skip_vendor() {
        let provider = JsFixProvider::new();
        assert!(!provider.should_skip_path(Path::new("/project/src/vendor/lib.ts")));
    }

    // -- is_whole_file_rename tests --

    #[test]
    fn test_whole_file_rename_with_imported_name() {
        let provider = JsFixProvider::new();
        let mut vars = BTreeMap::new();
        vars.insert(
            "importedName".to_string(),
            serde_json::Value::String("Chip".to_string()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        assert!(provider.is_whole_file_rename(&incident));
    }

    #[test]
    fn test_not_whole_file_rename_without_imported_name() {
        let provider = JsFixProvider::new();
        let mut vars = BTreeMap::new();
        vars.insert(
            "propName".to_string(),
            serde_json::Value::String("isActive".to_string()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        assert!(!provider.is_whole_file_rename(&incident));
    }

    // -- bracket_depth tests --

    #[test]
    fn test_bracket_depth_balanced() {
        assert_eq!(bracket_depth("{ foo: bar }"), 0);
        assert_eq!(bracket_depth("foo()"), 0);
        assert_eq!(bracket_depth("[1, 2, 3]"), 0);
        assert_eq!(bracket_depth("{ foo: [1, 2] }"), 0);
    }

    #[test]
    fn test_bracket_depth_open() {
        assert_eq!(bracket_depth("actions={["), 2);
        assert_eq!(bracket_depth("  <Button"), 0);
        assert_eq!(bracket_depth("foo(bar, {"), 2);
    }

    #[test]
    fn test_bracket_depth_close() {
        assert_eq!(bracket_depth("]}"), -2);
        assert_eq!(bracket_depth(")"), -1);
    }

    #[test]
    fn test_bracket_depth_ignores_string_literals() {
        assert_eq!(bracket_depth(r#"  foo="{not a bracket}""#), 0);
        assert_eq!(bracket_depth("  foo='[still not]'"), 0);
    }

    // -- dedup_import_specifiers tests --

    #[test]
    fn test_dedup_import_removes_duplicates() {
        let mut lines =
            vec!["import { Content, Content, Content } from '@patternfly/react-core';".to_string()];
        dedup_import_specifiers(&mut lines);
        let count = lines[0].matches("Content").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_dedup_import_preserves_different_specifiers() {
        let mut lines = vec!["import { Foo, Bar, Foo, Baz, Bar } from '@pkg';".to_string()];
        dedup_import_specifiers(&mut lines);
        assert_eq!(lines[0].matches("Foo").count(), 1);
        assert_eq!(lines[0].matches("Bar").count(), 1);
        assert_eq!(lines[0].matches("Baz").count(), 1);
    }

    // -- get_matched_text tests --

    #[test]
    fn test_get_matched_text_prop_name_first() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "propName".to_string(),
            serde_json::Value::String("isActive".to_string()),
        );
        vars.insert(
            "componentName".to_string(),
            serde_json::Value::String("Button".to_string()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        assert_eq!(get_matched_text_from_incident(&incident), "isActive");
    }

    #[test]
    fn test_get_matched_text_empty_when_no_known_vars() {
        let incident = make_test_incident("", 1, BTreeMap::new());
        assert_eq!(get_matched_text_from_incident(&incident), "");
    }

    // -- get_matched_text_for_rename tests --

    #[test]
    fn test_get_matched_text_for_rename_prefers_prop_name() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "propName".into(),
            serde_json::Value::String("spaceItems".into()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        let mappings = vec![RenameMapping {
            old: "spaceItems".into(),
            new: "gap".into(),
        }];
        assert_eq!(
            get_matched_text_for_rename_from_incident(&incident, &mappings),
            "spaceItems"
        );
    }

    #[test]
    fn test_get_matched_text_for_rename_falls_back_to_prop_value() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "propName".into(),
            serde_json::Value::String("variant".into()),
        );
        vars.insert(
            "propValue".into(),
            serde_json::Value::String("light".into()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        let mappings = vec![RenameMapping {
            old: "light".into(),
            new: "secondary".into(),
        }];
        assert_eq!(
            get_matched_text_for_rename_from_incident(&incident, &mappings),
            "light"
        );
    }

    // -- post_process_lines integration test --

    #[test]
    fn test_post_process_deduplicates_imports() {
        let provider = JsFixProvider::new();
        let mut lines = vec![
            "import { Content, Content } from '@patternfly/react-core';".to_string(),
            "const x = 1;".to_string(),
        ];
        provider.post_process_lines(&mut lines);
        assert_eq!(lines[0].matches("Content").count(), 1);
        assert_eq!(lines[1], "const x = 1;");
    }

    // -- npm resolution helper tests --

    #[test]
    fn test_extract_major() {
        assert_eq!(extract_major("^6.4.1"), 6);
        assert_eq!(extract_major("~5.0.0"), 5);
        assert_eq!(extract_major("6.4.1"), 6);
        assert_eq!(extract_major(">=7.0.0"), 7);
        assert_eq!(extract_major("^6.0.0-alpha.1"), 6);
    }

    #[test]
    fn test_parse_semver_tuple() {
        assert_eq!(parse_semver_tuple("6.4.1"), Some((6, 4, 1)));
        assert_eq!(parse_semver_tuple("5.0.0"), Some((5, 0, 0)));
        assert_eq!(parse_semver_tuple("6.0.0-alpha.1"), Some((6, 0, 0)));
    }

    // -- dependent incident detection test --

    #[test]
    fn test_dependent_incident_updates_correct_package() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_json = dir.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{
  "devDependencies": {
    "@patternfly/react-core": "^6.4.1",
    "@patternfly/react-topology": "5.2.1"
  }
}"#,
        )
        .unwrap();

        // Create a dependent incident (as the frontend-analyzer-provider would)
        let mut vars = BTreeMap::new();
        vars.insert(
            "dependencyName".into(),
            serde_json::Value::String("@patternfly/react-topology".into()),
        );
        vars.insert(
            "dependencyVersion".into(),
            serde_json::Value::String("5.2.1".into()),
        );
        vars.insert(
            "dependencyType".into(),
            serde_json::Value::String("devDependencies".into()),
        );
        vars.insert(
            "isDependentOf".into(),
            serde_json::Value::String("@patternfly/react-core".into()),
        );
        vars.insert(
            "dependentConstraint".into(),
            serde_json::Value::String("^5.1.1".into()),
        );

        let incident = make_test_incident(&format!("file://{}", pkg_json.display()), 4, vars);

        // Call the function — it will try to query npm for react-topology.
        // In CI without network, the npm query may fail, but the function
        // should gracefully return None rather than panic.
        let result = plan_ensure_npm_dependency(
            "semver-dep-update-patternfly-react-core",
            &incident,
            "@patternfly/react-core",
            "^6.4.1",
            &pkg_json,
        );

        // If npm is reachable, we get a fix targeting react-topology (not react-core).
        // If npm is unreachable, we get an empty vec (graceful degradation).
        if let Some(fix) = result.first() {
            assert!(
                fix.description.contains("react-topology"),
                "Fix should target react-topology, got: {}",
                fix.description
            );
            assert!(
                !fix.description.contains("react-core"),
                "Fix should NOT mention react-core as the package to update"
            );
        }
        // Either way: no panic, no crash.
    }

    // -- non-dependent incident preserves existing behavior --

    #[test]
    fn test_non_dependent_incident_uses_provided_version() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_json = dir.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{
  "dependencies": {
    "@patternfly/react-core": "5.3.4"
  }
}"#,
        )
        .unwrap();

        let vars = BTreeMap::new();
        let incident = make_test_incident(&format!("file://{}", pkg_json.display()), 3, vars);

        let result = plan_ensure_npm_dependency(
            "semver-dep-update-patternfly-react-core",
            &incident,
            "@patternfly/react-core",
            "^6.4.1",
            &pkg_json,
        );

        assert_eq!(
            result.len(),
            1,
            "Should produce exactly one fix for primary dep update"
        );
        let fix = &result[0];
        assert_eq!(fix.edits.len(), 1);
        assert!(fix.edits[0].new_text.contains("6.4.1"));
        assert!(fix.description.contains("react-core"));
    }

    #[test]
    fn parse_yarn_missing_peers_basic() {
        let output = "\
➤ YN0000: · Yarn 4.6.0
➤ YN0002: @patternfly/react-charts@npm:8.4.1 doesn't provide victory (p1a2b3), requested by @patternfly/react-charts.
➤ YN0002: @patternfly/react-charts@npm:8.4.1 doesn't provide victory-core (p4d5e6), requested by some-dep.
➤ YN0000: · Done in 1.5s
";
        let peers = super::parse_yarn_missing_peer_deps(output);
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&"victory".to_string()));
        assert!(peers.contains(&"victory-core".to_string()));
    }

    #[test]
    fn parse_yarn_missing_peers_with_ansi() {
        // Simulate ANSI color codes wrapping the warning code
        let output = "\x1b[33m➤\x1b[0m \x1b[33mYN0002\x1b[0m: \x1b[38;5;173mfoo@npm:1.0.0\x1b[0m doesn't provide \x1b[38;5;111mbar\x1b[0m (p7g8h9), requested by baz.\n";
        let peers = super::parse_yarn_missing_peer_deps(output);
        assert_eq!(peers, vec!["bar"]);
    }

    #[test]
    fn parse_yarn_missing_peers_scoped_package() {
        let output = "➤ YN0002: some-pkg@npm:2.0.0 doesn't provide @scope/peer-pkg (pabcde), requested by other-dep.\n";
        let peers = super::parse_yarn_missing_peer_deps(output);
        assert_eq!(peers, vec!["@scope/peer-pkg"]);
    }

    #[test]
    fn parse_yarn_missing_peers_deduplicates() {
        let output = "\
➤ YN0002: pkg-a@npm:1.0.0 doesn't provide victory (p11111), requested by dep-a.
➤ YN0002: pkg-b@npm:2.0.0 doesn't provide victory (p22222), requested by dep-b.
➤ YN0002: pkg-c@npm:3.0.0 doesn't provide victory (p33333), requested by dep-c.
";
        let peers = super::parse_yarn_missing_peer_deps(output);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0], "victory");
    }

    #[test]
    fn parse_yarn_missing_peers_no_warnings() {
        let output = "➤ YN0000: · Yarn 4.6.0\n➤ YN0000: · Done in 0.5s\n";
        let peers = super::parse_yarn_missing_peer_deps(output);
        assert!(peers.is_empty());
    }
}
