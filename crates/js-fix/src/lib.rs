//! JS/TS/JSX/TSX language-specific fix operations.
//!
//! Implements [`LanguageFixProvider`] for the JavaScript/TypeScript ecosystem:
//! - Skips `node_modules/` paths
//! - Deduplicates ES import specifiers after renames
//! - Removes JSX attributes (props) using syntax-aware regex
//! - Extracts matched text from JSX/React incident variables
//! - Manages `package.json` dependencies

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
    ) -> Option<PlannedFix> {
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
/// 1. If `file_path` is a `package.json`, use it directly (dependency condition leg).
///    Otherwise walk up from `file_path` to find the nearest `package.json`
///    (import condition leg -- the incident points at a source file).
/// 2. Try to update: scan for the package name in the file and replace the version.
/// 3. If not found: insert a new entry into the `"dependencies"` block.
fn plan_ensure_npm_dependency(
    rule_id: &str,
    _incident: &Incident,
    package: &str,
    new_version: &str,
    file_path: &Path,
) -> Option<PlannedFix> {
    // Resolve the target package.json
    let pkg_json = if file_path.file_name().is_some_and(|f| f == "package.json") {
        file_path.to_path_buf()
    } else {
        find_nearest_package_json(file_path)?
    };

    let source = std::fs::read_to_string(&pkg_json).ok()?;
    let pkg_json_uri = format!("file://{}", pkg_json.display());

    // --- Try update: find the package name and replace its version ---
    let package_quoted = format!("\"{}\"", package);
    let version_re = regex::Regex::new(r#"("[\^~><=]*\d+\.\d+\.\d+[^"]*")"#).ok()?;

    for (idx, file_line) in source.lines().enumerate() {
        if !file_line.contains(&package_quoted) {
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

    // --- Insert: package not found, add it to the "dependencies" block ---
    let lines: Vec<&str> = source.lines().collect();
    let mut in_dependencies = false;
    let mut brace_depth = 0;
    let mut last_entry_line: Option<usize> = None;
    let mut closing_brace_line: Option<usize> = None;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        if !in_dependencies {
            if trimmed.starts_with("\"dependencies\"") {
                in_dependencies = true;
                if trimmed.contains('{') {
                    brace_depth = 1;
                }
            }
            continue;
        }

        if brace_depth == 0 && trimmed.starts_with('{') {
            brace_depth = 1;
            continue;
        }

        if brace_depth == 1 {
            if trimmed == "}" || trimmed == "}," {
                closing_brace_line = Some(idx);
                break;
            }
            if !trimmed.is_empty() {
                last_entry_line = Some(idx);
            }
        }

        for ch in trimmed.chars() {
            if ch == '{' {
                brace_depth += 1;
            } else if ch == '}' {
                brace_depth -= 1;
                if brace_depth == 0 {
                    closing_brace_line = Some(idx);
                    break;
                }
            }
        }
        if closing_brace_line.is_some() {
            break;
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
}
