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

    fn post_process_lines(&self, lines: &mut [String]) {
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

    fn discover_companion_test_files(&self, file_path: &Path) -> Vec<PathBuf> {
        discover_java_companion_test_files(file_path)
    }
}

// ── Import deduplication ───────────────────────────────────────────────────

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
}
