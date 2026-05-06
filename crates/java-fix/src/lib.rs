//! Java language-specific fix operations.
//!
//! Implements [`LanguageFixProvider`] for the Java ecosystem:
//! - Skips `target/`, `build/`, `.gradle/`, `.mvn/` paths
//! - Deduplicates Java import statements after renames
//! - Removes Java annotations using regex-based pattern matching
//! - Extracts matched text from Java incident variables
//! - Manages `pom.xml` / `build.gradle` dependencies
//! - Discovers companion test files by Java naming conventions

use fix_engine::language::LanguageFixProvider;
use fix_engine_core::*;
use konveyor_core::incident::Incident;
use std::path::{Path, PathBuf};

/// Language fix provider for Java source files.
pub struct JavaFixProvider;

impl JavaFixProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JavaFixProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageFixProvider for JavaFixProvider {
    fn should_skip_path(&self, path: &Path) -> bool {
        // Skip build output, dependency caches, and IDE metadata
        path.components().any(|c| {
            let name = c.as_os_str().to_string_lossy();
            matches!(
                name.as_ref(),
                "target"
                    | "build"
                    | ".gradle"
                    | ".mvn"
                    | ".settings"
                    | ".idea"
                    | ".classpath"
                    | ".project"
                    | "bin"
                    | "out"
                    | "node_modules"
            )
        })
    }

    fn post_process_lines(&self, lines: &mut Vec<String>) {
        resolve_pending_imports(lines);
        dedup_java_imports(lines);
    }

    fn plan_remove_attribute(
        &self,
        rule_id: &str,
        incident: &Incident,
        file_path: &Path,
        report: &mut FixReport,
    ) -> Option<PlannedFix> {
        plan_remove_annotation(rule_id, incident, file_path, report)
    }

    fn plan_ensure_dependency(
        &self,
        rule_id: &str,
        incident: &Incident,
        package: &str,
        new_version: &str,
        file_path: &Path,
        report: &mut FixReport,
    ) -> Vec<PlannedFix> {
        plan_ensure_java_dependency(rule_id, incident, package, new_version, file_path, report)
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
        // Import-scope renames affect the whole file: the import statement itself
        // plus all usages of the imported class/interface throughout the file.
        incident.variables.contains_key("importedName")
            || incident.variables.contains_key("module")
    }

    fn plan_annotation_param_rewrite(
        &self,
        rule_id: &str,
        incident: &Incident,
        old_param: &str,
        new_param: &str,
        value_transform: &str,
        file_path: &Path,
        report: &mut FixReport,
    ) -> Option<PlannedFix> {
        plan_java_annotation_param_rewrite(
            rule_id,
            incident,
            old_param,
            new_param,
            value_transform,
            file_path,
            report,
        )
    }

    fn plan_import_rename(
        &self,
        rule_id: &str,
        incident: &Incident,
        old_fqn: &str,
        new_fqn: &str,
        file_path: &Path,
        report: &mut FixReport,
    ) -> Option<PlannedFix> {
        plan_java_import_rename(rule_id, incident, old_fqn, new_fqn, file_path, report)
    }

    fn discover_companion_test_files(&self, file_path: &Path) -> Vec<PathBuf> {
        discover_java_companion_test_files(file_path)
    }

    fn plan_proactive_dependency(
        &self,
        rule_id: &str,
        old_package: &str,
        new_package: &str,
        new_version: &str,
        project_root: &Path,
        report: &mut FixReport,
    ) -> Vec<PlannedFix> {
        plan_proactive_java_dependency(
            rule_id,
            old_package,
            new_package,
            new_version,
            project_root,
            report,
        )
    }

    fn plan_config_file_renames(
        &self,
        rule_id: &str,
        old_fqn: &str,
        new_fqn: &str,
        project_root: &Path,
        report: &mut FixReport,
    ) -> Vec<PlannedFix> {
        plan_config_file_fqn_renames(rule_id, old_fqn, new_fqn, project_root, report)
    }
}

// ── Import deduplication ───────────────────────────────────────────────────

/// Resolve `// __ADD_IMPORT:fqn` markers left by `AnnotationParamRewrite`.
///
/// Markers are embedded in annotation rewrite edits to avoid edit conflicts
/// with namespace migration edits on import lines. This function runs in
/// post-processing after all edits are applied:
/// 1. Scans all lines for `// __ADD_IMPORT:fqn` markers
/// 2. Strips markers from the lines
/// 3. Inserts missing imports after the last existing import line
fn resolve_pending_imports(lines: &mut Vec<String>) {
    let mut fqns_to_add: Vec<String> = Vec::new();

    // Scan for markers and strip them
    for line in lines.iter_mut() {
        let mut remaining = line.as_str();
        while let Some(marker_pos) = remaining.find("// __ADD_IMPORT:") {
            let after_marker = &remaining[marker_pos + "// __ADD_IMPORT:".len()..];
            // Extract the FQN (up to next space, marker, or end of line)
            let fqn_end = after_marker
                .find(" // __ADD_IMPORT:")
                .unwrap_or(after_marker.len());
            let fqn = after_marker[..fqn_end].trim().to_string();
            if !fqn.is_empty() {
                fqns_to_add.push(fqn);
            }
            remaining = &remaining[..marker_pos];
        }

        // Strip all markers from this line
        if let Some(pos) = line.find(" // __ADD_IMPORT:") {
            line.truncate(pos);
        }
    }

    if fqns_to_add.is_empty() {
        return;
    }

    // Dedup
    fqns_to_add.sort();
    fqns_to_add.dedup();

    // Find existing imports to avoid duplicates
    let existing_imports: std::collections::HashSet<String> = lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("import ") && t.ends_with(';')
        })
        .map(|l| l.trim().to_string())
        .collect();

    // Filter out already-imported FQNs
    let new_imports: Vec<String> = fqns_to_add
        .into_iter()
        .filter(|fqn| !existing_imports.contains(&format!("import {};", fqn)))
        .map(|fqn| format!("import {};", fqn))
        .collect();

    if new_imports.is_empty() {
        return;
    }

    // Find insertion point: after the last import line
    let insert_after = lines
        .iter()
        .enumerate()
        .rev()
        .find(|(_, l)| {
            let t = l.trim();
            t.starts_with("import ") && t.ends_with(';')
        })
        .map(|(i, _)| i + 1)
        .unwrap_or(1); // After package declaration if no imports

    // Insert the new imports
    for (offset, imp) in new_imports.into_iter().enumerate() {
        lines.insert(insert_after + offset, imp);
    }
}

/// Deduplicate Java import statements.
///
/// After renames, the same import may appear twice. This removes exact
/// duplicates while preserving order (keeps first occurrence).
fn dedup_java_imports(lines: &mut [String]) {
    let mut seen_imports = std::collections::HashSet::new();

    for line in lines.iter_mut() {
        let trimmed = line.trim();
        if trimmed.starts_with("import ") && trimmed.ends_with(';') {
            if !seen_imports.insert(trimmed.to_string()) {
                // Duplicate import -- blank it out
                *line = String::new();
            }
        }
    }
}

// ── Import rename ──────────────────────────────────────────────────────────

/// Plan a Java import + class rename.
///
/// Given old and new fully-qualified names (e.g., `org.hibernate.type.StringType`
/// → `org.hibernate.type.JavaObjectType`), this function:
///
/// 1. Replaces the import statement: `import old.Foo;` → `import new.Bar;`
/// 2. If the simple class name changed, does word-boundary-aware replacement
///    of the old class name with the new one throughout the file.
///
/// Word-boundary awareness prevents mangling unrelated identifiers — e.g.,
/// renaming `read` to `extract` won't turn `Thread` into `Thextract`.
fn plan_java_import_rename(
    rule_id: &str,
    incident: &Incident,
    old_fqn: &str,
    new_fqn: &str,
    file_path: &Path,
    report: &mut FixReport,
) -> Option<PlannedFix> {
    let source = std::fs::read_to_string(file_path).ok()?;
    let source_lines: Vec<&str> = source.lines().collect();

    if source_lines.is_empty() {
        return None;
    }

    // Extract simple class names from FQNs
    let old_class = old_fqn.rsplit('.').next().unwrap_or(old_fqn);
    let new_class = new_fqn.rsplit('.').next().unwrap_or(new_fqn);
    let class_changed = old_class != new_class;

    let mut edits = Vec::new();

    // Phase 1: Find and replace import statements containing old_fqn.
    // Handles both exact FQN imports (`import org.old.Foo;`) and
    // namespace prefix imports (`import javax.persistence.Entity;` when
    // old_fqn is `javax.persistence`).
    for (idx, line) in source_lines.iter().enumerate() {
        let trimmed = line.trim();
        if (trimmed.starts_with("import ") || trimmed.starts_with("import static "))
            && trimmed.contains(old_fqn)
        {
            let new_line = line.replace(old_fqn, new_fqn);
            if new_line != *line {
                edits.push(TextEdit {
                    line: (idx + 1) as u32,
                    old_text: line.to_string(),
                    new_text: new_line,
                    rule_id: rule_id.to_string(),
                    description: format!("Replace import: {} → {}", old_fqn, new_fqn),
                    replace_all: false,
                });
            }
        }
    }

    // Phase 2: If the class name changed, do word-boundary-aware replacement
    // in the rest of the file (skip import lines — already handled above)
    if class_changed {
        for (idx, line) in source_lines.iter().enumerate() {
            let trimmed = line.trim();
            // Skip import lines (already handled) and blank lines
            if trimmed.starts_with("import ") || trimmed.is_empty() {
                continue;
            }

            if let Some(new_line) = replace_at_word_boundary(line, old_class, new_class) {
                edits.push(TextEdit {
                    line: (idx + 1) as u32,
                    old_text: line.to_string(),
                    new_text: new_line,
                    rule_id: rule_id.to_string(),
                    description: format!("Rename class: {} → {}", old_class, new_class),
                    replace_all: false,
                });
            }
        }
    }

    if edits.is_empty() {
        report.record_skip(
            rule_id,
            &incident.file_uri,
            incident.line_number,
            SkipReason::TextNotFound,
            Some(format!("import {} not found in file", old_fqn)),
        );
        return None;
    }

    let description = if class_changed {
        format!("Rename import {} → {} and class {} → {}", old_fqn, new_fqn, old_class, new_class)
    } else {
        format!("Rename import {} → {}", old_fqn, new_fqn)
    };

    Some(PlannedFix {
        rule_id: rule_id.to_string(),
        file_uri: incident.file_uri.clone(),
        line: incident.line_number.unwrap_or(1),
        description,
        edits,
        source: FixSource::Pattern,
        confidence: FixConfidence::High,
    })
}

/// Replace all occurrences of `old` with `new` in `line`, but only at word
/// boundaries. Returns `None` if no replacement was made.
///
/// A word boundary means the character immediately before and after the match
/// must NOT be alphanumeric or underscore (`[a-zA-Z0-9_]`).
fn replace_at_word_boundary(line: &str, old: &str, new: &str) -> Option<String> {
    if old.is_empty() || !line.contains(old) {
        return None;
    }

    let mut result = String::with_capacity(line.len());
    let mut pos = 0;
    let line_bytes = line.as_bytes();
    let mut changed = false;

    while pos < line.len() {
        if let Some(match_start) = line[pos..].find(old) {
            let abs_start = pos + match_start;
            let abs_end = abs_start + old.len();

            // Check word boundary before the match
            let boundary_before = if abs_start == 0 {
                true
            } else {
                let c = line_bytes[abs_start - 1] as char;
                !c.is_alphanumeric() && c != '_'
            };

            // Check word boundary after the match
            let boundary_after = if abs_end >= line.len() {
                true
            } else {
                let c = line_bytes[abs_end] as char;
                !c.is_alphanumeric() && c != '_'
            };

            // Copy everything before the match
            result.push_str(&line[pos..abs_start]);

            if boundary_before && boundary_after {
                // Word boundary match — do the replacement
                result.push_str(new);
                changed = true;
            } else {
                // Not a word boundary — keep original text
                result.push_str(old);
            }

            pos = abs_end;
        } else {
            // No more matches — copy the rest
            result.push_str(&line[pos..]);
            break;
        }
    }

    if changed { Some(result) } else { None }
}

// ── Annotation parameter rewrite ───────────────────────────────────────────

/// Plan a Java annotation parameter rewrite.
///
/// Rewrites annotation element names and optionally transforms values.
/// For example: `@Type(type = "com.example.Foo")` → `@Type(value = Foo.class)`.
///
/// Supports two value transforms:
/// - `StringFqnToClassLiteral`: `"com.example.Foo"` → `Foo.class`
///   Also adds `import com.example.Foo;` if not already present.
///   Falls through to `None` if the referenced class doesn't exist (e.g.,
///   built-in types that were removed), allowing the LLM phase to handle it.
/// - `Identity`: keep the value as-is, just rename the element name.
fn plan_java_annotation_param_rewrite(
    rule_id: &str,
    incident: &Incident,
    old_param: &str,
    new_param: &str,
    value_transform: &str,
    file_path: &Path,
    report: &mut FixReport,
) -> Option<PlannedFix> {
    let source = std::fs::read_to_string(file_path).ok()?;
    let source_lines: Vec<&str> = source.lines().collect();

    let line_num = incident.line_number.unwrap_or(0);
    if line_num == 0 || source_lines.is_empty() {
        return None;
    }

    // Build a regex to match the annotation parameter.
    // Pattern: old_param = "value"
    let param_re = regex::Regex::new(&format!(
        r#"{}(\s*=\s*)"([^"]+)""#,
        regex::escape(old_param)
    ))
    .ok()?;

    // The incident fires on the IMPORT line, not the annotation usage.
    // Scan the ENTIRE file for annotation usages matching the pattern.
    let mut edits = Vec::new();
    let mut imports_to_add = std::collections::HashSet::new();

    for idx in 0..source_lines.len() {
        let line = source_lines[idx];
        // Quick check: must contain the old param and look like an annotation line
        if !line.contains(old_param) {
            continue;
        }
        if let Some(caps) = param_re.captures(line) {
            let spacing = &caps[1]; // the " = " part
            let fqn_value = &caps[2]; // the FQN string value

            let new_line = match value_transform {
                "StringFqnToClassLiteral" => {
                    // Extract simple class name from FQN
                    let simple_name = fqn_value.rsplit('.').next().unwrap_or(fqn_value);

                    // Build the replacement: old_param = "fqn" → new_param = SimpleName.class
                    let old_text = format!(
                        "{}{}\"{}\"",
                        old_param, spacing, fqn_value
                    );
                    let new_text = format!("{}{}{}.class", new_param, spacing, simple_name);
                    line.replace(&old_text, &new_text)
                }
                _ => {
                    // Identity: just rename the parameter, keep the value
                    line.replacen(old_param, new_param, 1)
                }
            };

            if new_line != line {
                edits.push(TextEdit {
                    line: (idx + 1) as u32,
                    old_text: line.to_string(),
                    new_text: new_line,
                    rule_id: rule_id.to_string(),
                    description: format!(
                        "Rewrite annotation param: {} → {}",
                        old_param, new_param
                    ),
                    replace_all: false,
                });
            }

            // For StringFqnToClassLiteral, check if the referenced class still exists.
            // Library types (org.hibernate.*) may have been removed in the new version —
            // fall through to LLM for these so it can determine the correct replacement
            // (e.g., NumericBooleanType → @JdbcTypeCode, yes_no → @Convert).
            if value_transform == "StringFqnToClassLiteral" {
                let fqn_value_str = fqn_value.to_string();
                // A consumer class must be a FQN (contains dots) and not a library type.
                // Short names without dots (e.g., "yes_no") are Hibernate shorthand type
                // names that should fall through to LLM.
                let is_consumer_class = fqn_value_str.contains('.')
                    && (fqn_value_str.contains("candlepin")
                        || fqn_value_str.contains("redhat")
                        || !fqn_value_str.starts_with("org.hibernate"));

                if is_consumer_class {
                    let import_line = format!("import {};", fqn_value_str);
                    let already_imported = source_lines
                        .iter()
                        .any(|l| l.trim() == import_line);

                    if !already_imported {
                        imports_to_add.insert(fqn_value_str);
                    }
                } else {
                    // Library type — the class may not exist in the new version.
                    // Don't rewrite this annotation; let the LLM handle it.
                    tracing::debug!(
                        rule_id = %rule_id,
                        fqn = %fqn_value_str,
                        "Annotation value references library type — skipping pattern fix for LLM"
                    );
                    // Remove the edit we just added for this line
                    edits.retain(|e| e.line != (idx + 1) as u32);
                }
            }
        }
    }

    // Add import markers to the FIRST annotation rewrite edit.
    // These markers are resolved by post_process_lines (resolve_pending_imports)
    // AFTER all edits are applied, avoiding conflicts with namespace migration
    // edits that may target the same import lines.
    if !imports_to_add.is_empty() {
        if let Some(first_edit) = edits.first_mut() {
            for fqn in &imports_to_add {
                first_edit
                    .new_text
                    .push_str(&format!(" // __ADD_IMPORT:{}", fqn));
            }
        }
    }

    if edits.is_empty() {
        report.record_skip(
            rule_id,
            &incident.file_uri,
            Some(line_num),
            SkipReason::TextNotFound,
            Some(format!(
                "Annotation param '{}' not found near line {}",
                old_param, line_num
            )),
        );
        return None;
    }

    Some(PlannedFix {
        rule_id: rule_id.to_string(),
        file_uri: incident.file_uri.clone(),
        line: line_num,
        description: format!(
            "Rewrite annotation: {} → {} ({})",
            old_param, new_param, value_transform
        ),
        edits,
        source: FixSource::Pattern,
        confidence: FixConfidence::High,
    })
}

// ── Annotation removal ─────────────────────────────────────────────────────

/// Plan removal of a Java annotation from a source file.
///
/// Handles:
/// - Simple annotations: `@Deprecated`
/// - Parameterized annotations: `@SuppressWarnings("unchecked")`
/// - Multi-line annotations: `@Entity(\n  name = "foo"\n)`
fn plan_remove_annotation(
    rule_id: &str,
    incident: &Incident,
    file_path: &Path,
    report: &mut FixReport,
) -> Option<PlannedFix> {
    let annotation_name = get_annotation_name(incident)?;

    let line = incident.line_number.unwrap_or(1) as usize;

    let source = std::fs::read_to_string(file_path).ok()?;
    let source_lines: Vec<&str> = source.lines().collect();

    if line == 0 || line > source_lines.len() {
        report.record_skip(
            rule_id,
            &incident.file_uri,
            incident.line_number,
            SkipReason::LineOutOfBounds,
            None,
        );
        return None;
    }

    let target_line = source_lines[line - 1];

    // Check if the annotation is on this line
    let annotation_pattern = format!("@{}", regex::escape(&annotation_name));
    let re = regex::Regex::new(&annotation_pattern).ok()?;

    if !re.is_match(target_line) {
        report.record_skip(
            rule_id,
            &incident.file_uri,
            incident.line_number,
            SkipReason::TextNotFound,
            Some(format!("@{} not found on line {}", annotation_name, line)),
        );
        return None;
    }

    // Build the full annotation pattern including optional parameters
    let full_pattern = format!(r"@{}\s*(?:\([^)]*\))?\s*\n?", regex::escape(&annotation_name));
    let full_re = regex::Regex::new(&full_pattern).ok()?;

    if let Some(m) = full_re.find(target_line) {
        let remaining = target_line[..m.start()].to_string() + &target_line[m.end()..];
        let remaining = remaining.trim().to_string();

        let description = format!("Remove @{} annotation", annotation_name);
        let edit = if remaining.is_empty() {
            // The annotation was the only thing on the line -- remove the entire line
            TextEdit {
                line: line as u32,
                old_text: target_line.to_string(),
                new_text: String::new(),
                rule_id: rule_id.to_string(),
                description: description.clone(),
                replace_all: false,
            }
        } else {
            // Remove just the annotation, keep the rest
            TextEdit {
                line: line as u32,
                old_text: target_line.to_string(),
                new_text: remaining,
                rule_id: rule_id.to_string(),
                description: description.clone(),
                replace_all: false,
            }
        };

        Some(PlannedFix {
            rule_id: rule_id.to_string(),
            file_uri: incident.file_uri.clone(),
            line: line as u32,
            description,
            edits: vec![edit],
            source: FixSource::Pattern,
            confidence: FixConfidence::High,
        })
    } else {
        None
    }
}

/// Extract the annotation name from incident variables.
fn get_annotation_name(incident: &Incident) -> Option<String> {
    // Try annotationName first (from java.referenced scope=ANNOTATION)
    if let Some(val) = incident.variables.get("annotationName") {
        return val.as_str().map(|s| s.to_string());
    }
    // Try matchingText as fallback -- strip leading '@' if present
    if let Some(val) = incident.variables.get("matchingText") {
        if let Some(s) = val.as_str() {
            let name = s.strip_prefix('@').unwrap_or(s);
            // Extract just the simple name (last segment after '.')
            let simple_name = name.rsplit('.').next().unwrap_or(name);
            return Some(simple_name.to_string());
        }
    }
    None
}

// ── Dependency management ──────────────────────────────────────────────────

/// Plan a dependency version update in `pom.xml` or `build.gradle`.
///
/// Currently a minimal implementation that returns empty (the engine will
/// route unhandled dependency incidents to LLM or manual review).
fn plan_ensure_java_dependency(
    _rule_id: &str,
    _incident: &Incident,
    _package: &str,
    _new_version: &str,
    _file_path: &Path,
    _report: &mut FixReport,
) -> Vec<PlannedFix> {
    // TODO: Implement pom.xml and build.gradle dependency version updates.
    // For now, dependency fixes are routed to LLM-assisted fixing or manual review.
    Vec::new()
}

/// Proactively scan Gradle/Maven manifest files for a dependency to update.
///
/// Called when no kantra incidents matched an `EnsureDependency` rule (e.g., because
/// kantra doesn't dispatch dependency conditions to external providers). Scans the
/// project root for `dependencies.gradle`, `build.gradle`, and `pom.xml` files
/// containing `old_package` and produces text edits to replace the coordinate.
///
/// `old_package` can be:
/// - A Maven coordinate prefix: `"org.hibernate:hibernate-c3p0"` → matches `"org.hibernate:hibernate-c3p0:5.6.15.Final"`
/// - A Java package name: `"javax.persistence"` → matches `"org.hibernate.javax.persistence:..."` (substring)
fn plan_proactive_java_dependency(
    rule_id: &str,
    old_package: &str,
    new_package: &str,
    new_version: &str,
    project_root: &Path,
    report: &mut FixReport,
) -> Vec<PlannedFix> {
    let mut fixes = Vec::new();

    // Find manifest files
    let manifest_names = [
        "dependencies.gradle",
        "build.gradle",
        "build.gradle.kts",
        "pom.xml",
    ];

    let mut manifest_files = Vec::new();
    // Search project root and immediate subdirectories
    for name in &manifest_names {
        let path = project_root.join(name);
        if path.exists() {
            manifest_files.push(path);
        }
    }
    // Also check common subdirectories
    if let Ok(entries) = std::fs::read_dir(project_root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                for name in &manifest_names {
                    let path = entry.path().join(name);
                    if path.exists() {
                        manifest_files.push(path);
                    }
                }
            }
        }
    }

    for manifest_path in &manifest_files {
        let source = match std::fs::read_to_string(manifest_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let source_lines: Vec<&str> = source.lines().collect();
        let mut edits = Vec::new();

        for (idx, line) in source_lines.iter().enumerate() {
            // Check if this line contains the old package/coordinate
            if !line.contains(old_package) {
                continue;
            }

            // For Maven coordinates (e.g., "org.hibernate:hibernate-c3p0"),
            // replace the coordinate + version
            if old_package.contains(':') {
                // old_package is a Maven coordinate like "org.hibernate:hibernate-c3p0"
                // new_package is like "org.hibernate.orm:hibernate-c3p0"
                // We need to also update the version
                let new_line = if new_package.contains(':') {
                    // Replace group:artifact and update version
                    let replaced = line.replace(old_package, new_package);
                    // Try to update the version in the same line
                    // Gradle format: "group:artifact:version"
                    if let Some(old_version_start) =
                        replaced.find(new_package).map(|p| p + new_package.len())
                    {
                        let after = &replaced[old_version_start..];
                        if after.starts_with(':') {
                            // Find the old version (between : and " or ')
                            if let Some(end) =
                                after[1..].find(|c: char| c == '"' || c == '\'')
                            {
                                let old_ver = &after[1..1 + end];
                                replaced.replace(
                                    &format!("{}:{}", new_package, old_ver),
                                    &format!("{}:{}", new_package, new_version),
                                )
                            } else {
                                replaced
                            }
                        } else {
                            replaced
                        }
                    } else {
                        replaced
                    }
                } else {
                    line.replace(old_package, new_package)
                };

                if new_line != *line {
                    edits.push(TextEdit {
                        line: (idx + 1) as u32,
                        old_text: line.to_string(),
                        new_text: new_line,
                        rule_id: rule_id.to_string(),
                        description: format!("Update dependency: {} → {}:{}", old_package, new_package, new_version),
                        replace_all: false,
                    });
                }
            } else {
                // old_package is a Java namespace like "javax.persistence"
                // The Gradle line might look like:
                //   libraries["hibernate"] = "org.hibernate.javax.persistence:hibernate-jpa-2.1-api:1.0.2.Final"
                // We need to replace the ENTIRE coordinate with the new one.
                // new_package format: "jakarta.persistence:jakarta.persistence-api"
                if new_package.contains(':') {
                    // Find the quoted coordinate in the line
                    let coord_re = regex::Regex::new(r#""([^"]+:[^"]+:[^"]+)""#).ok();
                    if let Some(re) = coord_re {
                        if let Some(caps) = re.captures(line) {
                            let full_coord = &caps[1];
                            if full_coord.contains(old_package) {
                                let new_coord = format!("{}:{}", new_package, new_version);
                                let new_line = line.replace(full_coord, &new_coord);
                                if new_line != *line {
                                    edits.push(TextEdit {
                                        line: (idx + 1) as u32,
                                        old_text: line.to_string(),
                                        new_text: new_line,
                                        rule_id: rule_id.to_string(),
                                        description: format!(
                                            "Update dependency: {} → {}:{}",
                                            full_coord, new_package, new_version
                                        ),
                                        replace_all: false,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        if !edits.is_empty() {
            let file_uri = format!("file://{}", manifest_path.display());
            tracing::info!(
                rule_id = %rule_id,
                file = %manifest_path.display(),
                edits = edits.len(),
                "Proactive dependency update"
            );
            fixes.push(PlannedFix {
                rule_id: rule_id.to_string(),
                file_uri,
                line: edits[0].line,
                description: format!(
                    "Proactive dependency update: {} → {}:{}",
                    old_package, new_package, new_version
                ),
                edits,
                source: FixSource::Pattern,
                confidence: FixConfidence::High,
            });
        }
    }

    if fixes.is_empty() {
        report.record_skip(
            rule_id,
            &format!("file://{}", project_root.display()),
            None,
            SkipReason::TextNotFound,
            Some(format!(
                "No manifest file found containing '{}'",
                old_package
            )),
        );
    }

    fixes
}

/// Scan config files for FQN string references and replace them.
///
/// Handles persistence.xml, application.properties, hibernate.cfg.xml,
/// and other config files that contain class FQN strings.
fn plan_config_file_fqn_renames(
    rule_id: &str,
    old_fqn: &str,
    new_fqn: &str,
    project_root: &Path,
    report: &mut FixReport,
) -> Vec<PlannedFix> {
    let mut fixes = Vec::new();
    let config_extensions = ["xml", "properties", "yml", "yaml", "cfg", "conf"];

    // Walk src/main/resources and project root for config files
    let search_dirs = [
        project_root.join("src/main/resources"),
        project_root.join("src/main/resources/META-INF"),
        project_root.to_path_buf(),
    ];

    for dir in &search_dirs {
        if !dir.exists() {
            continue;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if !config_extensions.contains(&ext) {
                continue;
            }

            let source = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if !source.contains(old_fqn) {
                continue;
            }

            let source_lines: Vec<&str> = source.lines().collect();
            let mut edits = Vec::new();

            for (idx, line) in source_lines.iter().enumerate() {
                if line.contains(old_fqn) {
                    let new_line = line.replace(old_fqn, new_fqn);
                    if new_line != *line {
                        edits.push(TextEdit {
                            line: (idx + 1) as u32,
                            old_text: line.to_string(),
                            new_text: new_line,
                            rule_id: rule_id.to_string(),
                            description: format!("Replace FQN: {} → {}", old_fqn, new_fqn),
                            replace_all: false,
                        });
                    }
                }
            }

            if !edits.is_empty() {
                let file_uri = format!("file://{}", path.display());
                fixes.push(PlannedFix {
                    rule_id: rule_id.to_string(),
                    file_uri,
                    line: edits[0].line,
                    description: format!("Config file FQN rename: {} → {}", old_fqn, new_fqn),
                    edits,
                    source: FixSource::Pattern,
                    confidence: FixConfidence::High,
                });
            }
        }
    }

    let _ = report; // Suppress unused warning
    fixes
}

// ── Incident variable extraction ───────────────────────────────────────────

/// Extract the primary matched text from a Java incident.
///
/// Java incidents carry these variables (from java-analyzer-provider):
/// - `matchingText`: always present, the full matched text
/// - `importedName`: import scope, the imported symbol name
/// - `typeName`: type reference scope
/// - `methodName`: method call scope
/// - `constructorName`: constructor call scope
/// - `annotationName`: annotation scope
/// - `fqn`: fully qualified name (when available)
fn get_matched_text_from_incident(incident: &Incident) -> String {
    // Priority order: specific scope variable > matchingText > empty
    let variable_names = [
        "importedName",
        "typeName",
        "methodName",
        "constructorName",
        "annotationName",
        "matchingText",
    ];

    for var_name in &variable_names {
        if let Some(val) = incident.variables.get(*var_name) {
            if let Some(s) = val.as_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }

    String::new()
}

/// Get the matched text for a rename operation.
///
/// Checks incident variables against the rename mappings to find which
/// mapping matches the incident.
fn get_matched_text_for_rename_from_incident(
    incident: &Incident,
    mappings: &[RenameMapping],
) -> String {
    // For Java renames, the primary match is the imported name or type name
    let variable_names = [
        "importedName",
        "typeName",
        "methodName",
        "constructorName",
        "annotationName",
        "matchingText",
    ];

    for var_name in &variable_names {
        if let Some(val) = incident.variables.get(*var_name) {
            if let Some(text) = val.as_str() {
                if !text.is_empty() {
                    // Check if any mapping's `old` matches this text
                    for mapping in mappings {
                        if text == mapping.old
                            || text.ends_with(&format!(".{}", mapping.old))
                            || text == mapping.new
                        {
                            return text.to_string();
                        }
                    }
                    // If no mapping matches exactly, return the text anyway
                    return text.to_string();
                }
            }
        }
    }

    String::new()
}

// ── Test file discovery ────────────────────────────────────────────────────

/// Discover companion test files for a Java source file.
///
/// Follows Java conventions:
/// - `src/main/java/com/example/Foo.java` → `src/test/java/com/example/FooTest.java`
/// - Also checks: `*Tests.java`, `*IT.java` (integration test)
/// - For Gradle: also checks `src/testFixtures/`
fn discover_java_companion_test_files(file_path: &Path) -> Vec<PathBuf> {
    let mut test_files = Vec::new();

    let file_stem = match file_path.file_stem() {
        Some(s) => s.to_string_lossy().to_string(),
        None => return test_files,
    };

    // Try standard Maven/Gradle test path convention
    let path_str = file_path.to_string_lossy();
    if path_str.contains("src/main/java/") {
        let test_base = path_str.replace("src/main/java/", "src/test/java/");
        let test_dir = Path::new(&test_base).parent();

        if let Some(dir) = test_dir {
            let suffixes = ["Test.java", "Tests.java", "IT.java"];
            for suffix in &suffixes {
                let test_path = dir.join(format!("{}{}", file_stem, suffix));
                if test_path.is_file() {
                    test_files.push(test_path);
                }
            }
        }
    }

    // Also check same directory (common for non-standard project layouts)
    if let Some(parent) = file_path.parent() {
        let suffixes = ["Test.java", "Tests.java", "IT.java"];
        for suffix in &suffixes {
            let test_path = parent.join(format!("{}{}", file_stem, suffix));
            if test_path.is_file() && !test_files.contains(&test_path) {
                test_files.push(test_path);
            }
        }
    }

    test_files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn make_incident(vars: Vec<(&str, &str)>) -> Incident {
        let mut variables = BTreeMap::new();
        for (k, v) in vars {
            variables.insert(k.to_string(), serde_json::Value::String(v.to_string()));
        }
        Incident {
            file_uri: "file:///test/Foo.java".to_string(),
            line_number: Some(10),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables,
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        }
    }

    #[test]
    fn test_should_skip_path() {
        let provider = JavaFixProvider::new();
        assert!(provider.should_skip_path(Path::new("/project/target/classes/Foo.class")));
        assert!(provider.should_skip_path(Path::new("/project/build/classes/Foo.class")));
        assert!(provider.should_skip_path(Path::new("/project/.gradle/caches/foo")));
        assert!(provider.should_skip_path(Path::new("/project/.mvn/wrapper/maven.jar")));
        assert!(!provider.should_skip_path(Path::new("/project/src/main/java/Foo.java")));
        assert!(!provider.should_skip_path(Path::new("/project/src/test/java/FooTest.java")));
    }

    #[test]
    fn test_get_matched_text_import() {
        let incident = make_incident(vec![
            ("importedName", "Session"),
            ("matchingText", "org.hibernate.Session"),
        ]);
        assert_eq!(get_matched_text_from_incident(&incident), "Session");
    }

    #[test]
    fn test_get_matched_text_type() {
        let incident = make_incident(vec![
            ("typeName", "SessionFactory"),
            ("matchingText", "org.hibernate.SessionFactory"),
        ]);
        assert_eq!(get_matched_text_from_incident(&incident), "SessionFactory");
    }

    #[test]
    fn test_get_matched_text_method() {
        let incident = make_incident(vec![
            ("methodName", "save"),
            ("matchingText", "org.hibernate.Session.save"),
        ]);
        assert_eq!(get_matched_text_from_incident(&incident), "save");
    }

    #[test]
    fn test_get_matched_text_annotation() {
        let incident = make_incident(vec![
            ("annotationName", "TypeDef"),
            ("matchingText", "org.hibernate.annotations.TypeDef"),
        ]);
        assert_eq!(get_matched_text_from_incident(&incident), "TypeDef");
    }

    #[test]
    fn test_get_matched_text_fallback() {
        let incident = make_incident(vec![("matchingText", "org.hibernate.Criteria")]);
        assert_eq!(
            get_matched_text_from_incident(&incident),
            "org.hibernate.Criteria"
        );
    }

    #[test]
    fn test_is_whole_file_rename() {
        let provider = JavaFixProvider::new();

        let import_incident = make_incident(vec![
            ("importedName", "Session"),
            ("module", "org.hibernate"),
        ]);
        assert!(provider.is_whole_file_rename(&import_incident));

        let type_incident = make_incident(vec![("typeName", "Session")]);
        assert!(!provider.is_whole_file_rename(&type_incident));
    }

    #[test]
    fn test_dedup_java_imports() {
        let mut lines = vec![
            "import org.hibernate.Session;".to_string(),
            "import org.hibernate.SessionFactory;".to_string(),
            "import org.hibernate.Session;".to_string(),
            "import jakarta.persistence.Entity;".to_string(),
        ];
        dedup_java_imports(&mut lines);
        assert_eq!(lines[0], "import org.hibernate.Session;");
        assert_eq!(lines[1], "import org.hibernate.SessionFactory;");
        assert_eq!(lines[2], ""); // duplicate removed
        assert_eq!(lines[3], "import jakarta.persistence.Entity;");
    }

    #[test]
    fn test_get_annotation_name() {
        let incident = make_incident(vec![("annotationName", "TypeDef")]);
        assert_eq!(get_annotation_name(&incident), Some("TypeDef".to_string()));

        let incident2 = make_incident(vec![("matchingText", "org.hibernate.annotations.TypeDef")]);
        assert_eq!(get_annotation_name(&incident2), Some("TypeDef".to_string()));

        let incident3 = make_incident(vec![("matchingText", "@TypeDef")]);
        assert_eq!(get_annotation_name(&incident3), Some("TypeDef".to_string()));
    }

    #[test]
    fn test_get_matched_text_for_rename() {
        let incident = make_incident(vec![("importedName", "Criteria")]);
        let mappings = vec![RenameMapping {
            old: "Criteria".to_string(),
            new: "CriteriaQuery".to_string(),
        }];
        assert_eq!(
            get_matched_text_for_rename_from_incident(&incident, &mappings),
            "Criteria"
        );
    }

    // ── replace_at_word_boundary tests ──────────────────────────────────

    #[test]
    fn test_word_boundary_simple_match() {
        let result = replace_at_word_boundary("StringType x = new StringType();", "StringType", "JavaObjectType");
        assert_eq!(result, Some("JavaObjectType x = new JavaObjectType();".to_string()));
    }

    #[test]
    fn test_word_boundary_no_substring_match() {
        // "read" should NOT match inside "Thread"
        let result = replace_at_word_boundary("List<Thread> threads = new ArrayList<>();", "read", "extract");
        assert_eq!(result, None);
    }

    #[test]
    fn test_word_boundary_no_match_in_already() {
        // "read" should NOT match inside "Already"
        let result = replace_at_word_boundary("throw new AlreadyInitializedException();", "read", "extract");
        assert_eq!(result, None);
    }

    #[test]
    fn test_word_boundary_no_match_in_readonly() {
        // "read" should NOT match inside "readOnly" because 'O' is alphanumeric after
        let result = replace_at_word_boundary("initValues.readOnly = false;", "read", "extract");
        assert_eq!(result, None);
    }

    #[test]
    fn test_word_boundary_standalone_match() {
        // "read" should match when it stands alone
        let result = replace_at_word_boundary("Object result = scrollable.read(0);", "read", "extract");
        assert_eq!(result, Some("Object result = scrollable.extract(0);".to_string()));
    }

    #[test]
    fn test_word_boundary_at_line_start() {
        let result = replace_at_word_boundary("StringType foo;", "StringType", "JavaObjectType");
        assert_eq!(result, Some("JavaObjectType foo;".to_string()));
    }

    #[test]
    fn test_word_boundary_at_line_end() {
        let result = replace_at_word_boundary("return new StringType", "StringType", "JavaObjectType");
        assert_eq!(result, Some("return new JavaObjectType".to_string()));
    }

    #[test]
    fn test_word_boundary_connection_not_in_jdbc_connection_access() {
        // "connection" should NOT match inside "JdbcConnectionAccess"
        let result = replace_at_word_boundary(
            "import org.hibernate.engine.jdbc.connections.spi.JdbcConnectionAccess;",
            "connection",
            "getSession",
        );
        assert_eq!(result, None);
    }

    #[test]
    fn test_word_boundary_connection_standalone() {
        let result = replace_at_word_boundary("Connection conn = session.connection();", "connection", "getSession");
        assert_eq!(result, Some("Connection conn = session.getSession();".to_string()));
    }

    #[test]
    fn test_word_boundary_no_match_returns_none() {
        let result = replace_at_word_boundary("int x = 42;", "StringType", "JavaObjectType");
        assert_eq!(result, None);
    }

    #[test]
    fn test_word_boundary_multiple_matches() {
        let result = replace_at_word_boundary(
            "StringType a = StringType.valueOf(StringType.class);",
            "StringType",
            "JavaObjectType",
        );
        assert_eq!(
            result,
            Some("JavaObjectType a = JavaObjectType.valueOf(JavaObjectType.class);".to_string())
        );
    }

    #[test]
    fn test_word_boundary_dot_is_boundary() {
        // A dot should be a word boundary
        let result = replace_at_word_boundary("session.read(0)", "read", "extract");
        assert_eq!(result, Some("session.extract(0)".to_string()));
    }

    #[test]
    fn test_word_boundary_underscore_is_not_boundary() {
        // Underscore is NOT a word boundary (part of identifier)
        let result = replace_at_word_boundary("my_read_method()", "read", "extract");
        assert_eq!(result, None);
    }

    // ── Annotation param rewrite tests ─────────────────────────────────────

    #[test]
    fn test_annotation_param_rewrite_string_fqn_to_class_literal() {
        // Test the core case: @Type(type = "com.example.MyType") → @Type(value = MyType.class)
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Foo.java");
        std::fs::write(
            &file,
            r#"package com.example;

import org.hibernate.annotations.Type;

public class Foo {
    @Type(type = "org.candlepin.hibernate.EmptyStringUserType")
    private String name;
}
"#,
        )
        .unwrap();

        let incident = Incident {
            file_uri: format!("file://{}", file.display()),
            line_number: Some(6),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables: BTreeMap::new(),
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        };
        let mut report = FixReport::new();

        let fix = plan_java_annotation_param_rewrite(
            "test-rule",
            &incident,
            "type",
            "value",
            "StringFqnToClassLiteral",
            &file,
            &mut report,
        );

        assert!(fix.is_some(), "Expected a planned fix");
        let fix = fix.unwrap();
        assert_eq!(fix.source, FixSource::Pattern);
        assert_eq!(fix.confidence, FixConfidence::High);

        // Should have at least an annotation rewrite edit
        let annotation_edit = fix
            .edits
            .iter()
            .find(|e| e.description.contains("Rewrite annotation param"));
        assert!(annotation_edit.is_some(), "Missing annotation param edit");
        let edit = annotation_edit.unwrap();
        assert!(
            edit.new_text.contains("value = EmptyStringUserType.class"),
            "Expected class literal, got: {}",
            edit.new_text
        );
        assert!(
            !edit.new_text.contains(r#""org.candlepin"#),
            "Should not have FQN string anymore"
        );

        // Should have an import marker in the first edit's new_text
        // (markers are resolved by post_process_lines, not as separate edits)
        assert!(
            edit.new_text.contains("// __ADD_IMPORT:org.candlepin.hibernate.EmptyStringUserType"),
            "Expected import marker in edit, got: {}",
            edit.new_text
        );
    }

    #[test]
    fn test_annotation_param_rewrite_library_type_falls_through() {
        // When the FQN is a library type (org.hibernate.*), the pattern fix
        // should return None — the class may have been removed in the new version.
        // The LLM phase will handle the correct replacement.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Bar.java");
        std::fs::write(
            &file,
            r#"package com.example;

import org.hibernate.annotations.Type;

public class Bar {
    @Type(type = "org.hibernate.type.NumericBooleanType")
    private boolean locked;
}
"#,
        )
        .unwrap();

        let incident = Incident {
            file_uri: format!("file://{}", file.display()),
            line_number: Some(6),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables: BTreeMap::new(),
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        };
        let mut report = FixReport::new();

        let fix = plan_java_annotation_param_rewrite(
            "test-rule",
            &incident,
            "type",
            "value",
            "StringFqnToClassLiteral",
            &file,
            &mut report,
        );

        // Library types should fall through to LLM (no edits → returns None)
        assert!(
            fix.is_none(),
            "Library types should fall through to LLM"
        );
    }

    #[test]
    fn test_annotation_param_rewrite_identity() {
        // Test identity transform: just rename the param, keep the value
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Baz.java");
        std::fs::write(
            &file,
            r#"package com.example;

public class Baz {
    @SomeAnnotation(oldName = "someValue")
    private String field;
}
"#,
        )
        .unwrap();

        let incident = Incident {
            file_uri: format!("file://{}", file.display()),
            line_number: Some(4),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables: BTreeMap::new(),
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        };
        let mut report = FixReport::new();

        let fix = plan_java_annotation_param_rewrite(
            "test-rule",
            &incident,
            "oldName",
            "newName",
            "Identity",
            &file,
            &mut report,
        );

        assert!(fix.is_some(), "Expected a planned fix for identity rename");
        let fix = fix.unwrap();
        let edit = fix.edits.first().unwrap();
        assert!(
            edit.new_text.contains("newName"),
            "Expected renamed param in: {}",
            edit.new_text
        );
        assert!(
            !edit.new_text.contains("oldName"),
            "Old param should be gone in: {}",
            edit.new_text
        );
    }
}
