//! Fix engine core types.
//!
//! Defines the data model for planned fixes: text edits grouped by file,
//! with support for pattern-based (deterministic) and LLM-assisted fixes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Return type for [`load_strategies_and_families`]: `(strategies, family_entries)`.
pub type StrategiesAndFamilies = (
    BTreeMap<String, FixStrategy>,
    BTreeMap<String, FixStrategyEntry>,
);

// Re-export shared types from konveyor-core so existing code continues to compile.
pub use konveyor_core::fix::{
    FixConfidence, FixSource, FixStrategyEntry, MappingEntry as StrategyMappingEntry,
    MemberMappingEntry,
};

// Re-export konveyor-core types for convenience
pub use konveyor_core::incident;
pub use konveyor_core::report;
pub use konveyor_core::rule;

/// A single text replacement within a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEdit {
    /// 1-indexed line number where the edit applies.
    pub line: u32,
    /// The original text to find on this line.
    pub old_text: String,
    /// The replacement text.
    pub new_text: String,
    /// Rule ID that generated this fix.
    pub rule_id: String,
    /// Human-readable description of what this fix does.
    pub description: String,
    /// When true, replace ALL occurrences of old_text on this line (not just the first).
    /// Used for prefix replacements (e.g. CssVariablePrefix) where a single line
    /// may contain multiple instances of the old prefix.
    #[serde(default)]
    pub replace_all: bool,
}

/// A planned fix for a single incident.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedFix {
    /// The text edits to apply.
    pub edits: Vec<TextEdit>,
    /// Confidence level.
    pub confidence: FixConfidence,
    /// How the fix was generated.
    pub source: FixSource,
    /// The rule ID this fix addresses.
    pub rule_id: String,
    /// File URI from the incident.
    pub file_uri: String,
    /// Line number from the incident.
    pub line: u32,
    /// Description of what this fix does.
    pub description: String,
}

/// A fix plan: all planned fixes grouped by file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FixPlan {
    /// Fixes grouped by file path.
    pub files: BTreeMap<PathBuf, Vec<PlannedFix>>,
    /// Incidents that could not be auto-fixed and need manual attention.
    pub manual: Vec<ManualFixItem>,
    /// Incidents pending LLM-assisted fix.
    pub pending_llm: Vec<LlmFixRequest>,
    /// Number of edits removed during plan-time deduplication because a more
    /// specific edit on the same line already covers the same text region.
    #[serde(default)]
    pub edits_subsumed: usize,
}

/// An incident that requires manual fixing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualFixItem {
    pub rule_id: String,
    pub file_uri: String,
    pub line: u32,
    pub message: String,
    pub code_snip: Option<String>,
}

/// A request to send to the LLM for fix generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmFixRequest {
    pub rule_id: String,
    pub file_uri: String,
    pub file_path: PathBuf,
    pub line: u32,
    pub message: String,
    pub code_snip: Option<String>,
    /// The full source content of the file (for context).
    pub source: Option<String>,
    /// Labels from the violation (e.g., "family=Modal", "change-type=prop-to-child").
    /// Used to coalesce related rules into coherent migration groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
}

/// Result of applying a fix plan.
#[derive(Debug, Default)]
pub struct FixResult {
    /// Number of files modified.
    pub files_modified: usize,
    /// Number of edits applied.
    pub edits_applied: usize,
    /// Number of edits skipped (line out of bounds or text not found).
    pub edits_skipped: usize,
    /// Number of edits subsumed by a more specific edit on the same line.
    pub edits_subsumed: usize,
    /// Errors encountered.
    pub errors: Vec<String>,
    /// Paths of files that were actually modified on disk.
    pub modified_files: Vec<std::path::PathBuf>,
}

/// A rename mapping: old name -> new name.
/// Used for prop renames, component renames, import renames, etc.
#[derive(Debug, Clone)]
pub struct RenameMapping {
    pub old: String,
    pub new: String,
}

/// Known fix strategies keyed by rule ID.
/// Each entry defines how to transform incidents from that rule into text edits.
#[derive(Debug, Clone)]
pub enum FixStrategy {
    /// Simple text replacement: rename the matched text.
    /// The mapping is propName/componentName/importedName old -> new.
    Rename(Vec<RenameMapping>),
    /// Remove the matched attribute (e.g., delete the entire prop from a JSX tag,
    /// or remove a decorator in Python, etc.).
    RemoveAttribute,
    /// Replace an import/include source path.
    ImportPathChange { old_path: String, new_path: String },
    /// Replace a CSS variable/class prefix.
    CssVariablePrefix {
        old_prefix: String,
        new_prefix: String,
        /// CSS classes to exclude from this prefix swap (dead after swap).
        exclude_patterns: Vec<String>,
    },
    /// Ensure a dependency exists at the correct version in the project manifest
    /// (e.g., package.json for Node.js, Cargo.toml for Rust, go.mod for Go).
    EnsureDependency {
        package: String,
        new_version: String,
    },
    /// No auto-fix available -- flag for manual review.
    Manual,
    /// Send to LLM for fix generation.
    /// `context` carries pre-formatted strategy data (strategy type, from/to,
    /// mappings, target structure, etc.) from the `FixStrategyEntry`.
    /// `None` when the rule fell through to Llm via label inference or default.
    Llm { context: Option<String> },
}

/// Convert a `FixStrategyEntry` (from the shared `konveyor-core` crate) to
/// a runtime `FixStrategy`.
///
/// When `mappings` is populated (consolidated rule), builds a multi-mapping
/// `FixStrategy::Rename` or extracts multiple `RemoveProp` targets.
pub fn strategy_entry_to_fix_strategy(entry: &FixStrategyEntry) -> FixStrategy {
    match entry.strategy.as_str() {
        "Rename" => {
            let mut renames: Vec<RenameMapping> = Vec::new();
            // Collect from mappings array (consolidated rule)
            for m in &entry.mappings {
                if let (Some(from), Some(to)) = (&m.from, &m.to) {
                    renames.push(RenameMapping {
                        old: from.clone(),
                        new: to.clone(),
                    });
                }
            }
            // Fall back to top-level from/to (single-rule strategy)
            if renames.is_empty() {
                if let (Some(from), Some(to)) = (&entry.from, &entry.to) {
                    renames.push(RenameMapping {
                        old: from.clone(),
                        new: to.clone(),
                    });
                }
            }
            if renames.is_empty() {
                FixStrategy::Manual
            } else {
                FixStrategy::Rename(renames)
            }
        }
        "RemoveProp" => FixStrategy::RemoveAttribute,
        "CssVariablePrefix" => {
            if let (Some(from), Some(to)) = (&entry.from, &entry.to) {
                FixStrategy::CssVariablePrefix {
                    old_prefix: from.clone(),
                    new_prefix: to.clone(),
                    exclude_patterns: entry.exclude_patterns.clone(),
                }
            } else {
                FixStrategy::Manual
            }
        }
        "ImportPathChange" => {
            if let (Some(from), Some(to)) = (&entry.from, &entry.to) {
                FixStrategy::ImportPathChange {
                    old_path: from.clone(),
                    new_path: to.clone(),
                }
            } else {
                FixStrategy::Manual
            }
        }
        "EnsureDependency" => {
            if let (Some(package), Some(new_version)) = (&entry.package, &entry.new_version) {
                FixStrategy::EnsureDependency {
                    package: package.clone(),
                    new_version: new_version.clone(),
                }
            } else {
                FixStrategy::Manual
            }
        }
        "PropValueChange" | "PropTypeChange" => FixStrategy::Llm {
            context: Some(format_strategy_context(entry)),
        },
        "LlmAssisted" => FixStrategy::Llm {
            context: Some(format_strategy_context(entry)),
        },
        // v2 SD-pipeline strategies -- these require structural
        // transformations that only the LLM can handle.
        "ChildToProp"
        | "PropToChild"
        | "PropToChildren"
        | "CompositionChange"
        | "DeprecatedMigration" => FixStrategy::Llm {
            context: Some(format_strategy_context(entry)),
        },
        // Family-level migration: the entry carries the complete target
        // component structure. Format it as a rich context block.
        "FamilyMigration" => FixStrategy::Llm {
            context: Some(format_family_migration_context(entry)),
        },
        _ => FixStrategy::Manual,
    }
}

/// Format a per-rule `FixStrategyEntry` into a human-readable context block
/// for the LLM prompt. Includes the strategy type, from/to mappings,
/// component/prop targets, member mappings, etc.
fn format_strategy_context(entry: &FixStrategyEntry) -> String {
    let mut parts = Vec::new();
    parts.push(format!("Strategy: {}", entry.strategy));
    if let Some(ref c) = entry.component {
        parts.push(format!("Component: {}", c));
    }
    if let Some(ref p) = entry.prop {
        parts.push(format!("Prop: {}", p));
    }
    if let Some(ref from) = entry.from {
        parts.push(format!("From: {}", from));
    }
    if let Some(ref to) = entry.to {
        parts.push(format!("To: {}", to));
    }
    if let Some(ref repl) = entry.replacement {
        parts.push(format!("Replacement: {}", repl));
    }
    if !entry.mappings.is_empty() {
        let maps: Vec<String> = entry
            .mappings
            .iter()
            .filter_map(|m| match (&m.from, &m.to) {
                (Some(f), Some(t)) => Some(format!("  {} -> {}", f, t)),
                _ => None,
            })
            .collect();
        if !maps.is_empty() {
            parts.push(format!("Mappings:\n{}", maps.join("\n")));
        }
    }
    if !entry.member_mappings.is_empty() {
        let maps: Vec<String> = entry
            .member_mappings
            .iter()
            .map(|m| format!("  {} -> {}", m.old_name, m.new_name))
            .collect();
        parts.push(format!("Member mappings:\n{}", maps.join("\n")));
    }
    if !entry.removed_members.is_empty() {
        parts.push(format!(
            "Removed members: {}",
            entry.removed_members.join(", ")
        ));
    }
    parts.join("\n")
}

/// Format a family-level `FixStrategyEntry` (keyed `family:<Name>`) into a
/// rich context block for the LLM prompt.
fn format_family_migration_context(entry: &FixStrategyEntry) -> String {
    let mut parts = Vec::new();
    parts.push("Strategy: FamilyMigration".to_string());

    if let Some(ref target) = entry.target_structure {
        parts.push(format!(
            "\nTarget structure (correct v6 composition):\n```jsx\n{}\n```",
            target
        ));
    }
    if let Some(ref comp) = entry.component {
        parts.push(format!("Component: {}", comp));
    }
    if !entry.retained_props.is_empty() {
        parts.push(format!(
            "Props that stay on root: {}",
            entry.retained_props.join(", ")
        ));
    }
    if !entry.prop_to_child.is_empty() {
        let maps: Vec<String> = entry
            .prop_to_child
            .iter()
            .map(|(prop, child)| format!("  {} -> <{} />", prop, child))
            .collect();
        parts.push(format!(
            "Props that move to child components:\n{}",
            maps.join("\n")
        ));
    }
    if !entry.child_props_to_parent.is_empty() {
        let maps: Vec<String> = entry
            .child_props_to_parent
            .iter()
            .map(|(child_prop, parent_prop)| format!("  {} -> {}", child_prop, parent_prop))
            .collect();
        parts.push(format!(
            "Child props that move to parent:\n{}",
            maps.join("\n")
        ));
    }
    if !entry.removed_children.is_empty() {
        parts.push(format!(
            "Removed children: {}",
            entry.removed_children.join(", ")
        ));
    }
    if !entry.new_imports.is_empty() {
        let src = entry.import_source.as_deref().unwrap_or("(same package)");
        parts.push(format!(
            "Add imports: {} from '{}'",
            entry.new_imports.join(", "),
            src
        ));
    }
    if !entry.removed_imports.is_empty() {
        parts.push(format!(
            "Remove imports: {}",
            entry.removed_imports.join(", ")
        ));
    }
    if !entry.prop_value_changes.is_empty() {
        let mut lines = Vec::new();
        for (prop, mappings) in &entry.prop_value_changes {
            for m in mappings {
                if let (Some(from), Some(to)) = (&m.from, &m.to) {
                    lines.push(format!("  {}: {} -> {}", prop, from, to));
                }
            }
        }
        if !lines.is_empty() {
            parts.push(format!("Prop value changes:\n{}", lines.join("\n")));
        }
    }
    if !entry.prop_type_changes.is_empty() {
        let mut lines = Vec::new();
        for (prop, mappings) in &entry.prop_type_changes {
            for m in mappings {
                match (&m.from, &m.to) {
                    (Some(from), Some(to)) => {
                        lines.push(format!("  {}: {} -> {}", prop, from, to));
                    }
                    (None, Some(to)) => {
                        lines.push(format!("  {} (current signature): {}", prop, to));
                    }
                    _ => {}
                }
            }
        }
        if !lines.is_empty() {
            parts.push(format!("Prop type changes:\n{}", lines.join("\n")));
        }
    }

    // Deprecated -> v6 migration context: complete prop mapping with types.
    if let Some(ref dm) = entry.deprecated_migration {
        parts.push(format!(
            "\nDeprecated -> v6 migration:\n  Old import: {}\n  New import: {}",
            dm.old_package, dm.new_package
        ));

        // Matching props (survived the migration, may have type changes)
        if !dm.matching_props.is_empty() {
            let mut lines = Vec::new();
            for p in &dm.matching_props {
                if p.type_changed {
                    lines.push(format!(
                        "  {} -> {} (TYPE CHANGED):\n    old: {}\n    new: {}",
                        p.old_name,
                        p.new_name,
                        p.old_type.as_deref().unwrap_or("?"),
                        p.new_type.as_deref().unwrap_or("?")
                    ));
                } else {
                    let typ = p.new_type.as_deref().unwrap_or("?");
                    if p.old_name == p.new_name {
                        lines.push(format!("  {}: {} (unchanged)", p.new_name, typ));
                    } else {
                        lines.push(format!(
                            "  {} -> {}: {} (renamed, type unchanged)",
                            p.old_name, p.new_name, typ
                        ));
                    }
                }
            }
            parts.push(format!("Matching props:\n{}", lines.join("\n")));
        }

        // New props (only on v6, not on deprecated)
        if !dm.new_props.is_empty() {
            let mut lines = Vec::new();
            for (name, typ) in &dm.new_props {
                let mut line = format!("  {}: {}", name, typ);
                // Auto-detect render-prop patterns
                if (typ.contains("=> React.ReactNode")
                    || typ.contains("=> ReactNode")
                    || typ.contains("=> React.ReactElement"))
                    && typ.contains("=>")
                {
                    line.push_str(
                        "\n    NOTE: This is a render function. Pass a function REFERENCE, \
                         not a function call.",
                    );
                }
                lines.push(line);
            }
            parts.push(format!(
                "New props on v6 (not on deprecated version):\n{}",
                lines.join("\n")
            ));
        }

        // Removed props (only on deprecated, no v6 equivalent)
        if !dm.removed_props.is_empty() {
            parts.push(format!(
                "Removed props (no v6 equivalent): {}",
                dm.removed_props.join(", ")
            ));
        }
    }

    parts.join("\n")
}

/// Load fix strategies from a JSON file and also extract raw family-level
/// `FixStrategyEntry` entries (keyed `family:*`) for use in family consolidation.
///
/// Returns `(strategies, family_entries)`.
pub fn load_strategies_and_families(
    path: &Path,
) -> Result<StrategiesAndFamilies, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let entries: BTreeMap<String, FixStrategyEntry> = serde_json::from_str(&content)?;
    let strategies = entries
        .iter()
        .map(|(rule_id, entry)| (rule_id.clone(), strategy_entry_to_fix_strategy(entry)))
        .collect();
    let families = entries
        .into_iter()
        .filter(|(k, _)| k.starts_with("family:"))
        .collect();
    Ok((strategies, families))
}

/// Load fix strategies from a JSON file.
///
/// Returns a map of rule_id -> FixStrategy.
pub fn load_strategies_from_json(
    path: &Path,
) -> Result<BTreeMap<String, FixStrategy>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let entries: BTreeMap<String, FixStrategyEntry> = serde_json::from_str(&content)?;
    let strategies = entries
        .iter()
        .map(|(rule_id, entry)| (rule_id.clone(), strategy_entry_to_fix_strategy(entry)))
        .collect();
    Ok(strategies)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_strategy_entry(strategy: &str) -> FixStrategyEntry {
        FixStrategyEntry::new(strategy)
    }

    #[test]
    fn test_rename_with_top_level_from_to() {
        let mut entry = make_strategy_entry("Rename");
        entry.from = Some("Chip".to_string());
        entry.to = Some("Label".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Rename(mappings) => {
                assert_eq!(mappings.len(), 1);
                assert_eq!(mappings[0].old, "Chip");
                assert_eq!(mappings[0].new, "Label");
            }
            other => panic!("Expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn test_rename_with_mappings_array() {
        let mut entry = make_strategy_entry("Rename");
        entry.mappings = vec![
            StrategyMappingEntry {
                from: Some("Chip".to_string()),
                to: Some("Label".to_string()),
                component: None,
                prop: None,
            },
            StrategyMappingEntry {
                from: Some("ChipGroup".to_string()),
                to: Some("LabelGroup".to_string()),
                component: None,
                prop: None,
            },
        ];

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Rename(mappings) => {
                assert_eq!(mappings.len(), 2);
                assert_eq!(mappings[0].old, "Chip");
                assert_eq!(mappings[0].new, "Label");
                assert_eq!(mappings[1].old, "ChipGroup");
                assert_eq!(mappings[1].new, "LabelGroup");
            }
            other => panic!("Expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn test_rename_mappings_take_precedence_over_top_level() {
        let mut entry = make_strategy_entry("Rename");
        entry.from = Some("TopLevel".to_string());
        entry.to = Some("ShouldBeIgnored".to_string());
        entry.mappings = vec![StrategyMappingEntry {
            from: Some("FromMapping".to_string()),
            to: Some("ToMapping".to_string()),
            component: None,
            prop: None,
        }];

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Rename(mappings) => {
                assert_eq!(mappings.len(), 1);
                assert_eq!(mappings[0].old, "FromMapping");
            }
            other => panic!("Expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn test_css_variable_prefix() {
        let mut entry = make_strategy_entry("CssVariablePrefix");
        entry.from = Some("pf-v5-".to_string());
        entry.to = Some("pf-v6-".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::CssVariablePrefix {
                old_prefix,
                new_prefix,
                exclude_patterns,
            } => {
                assert_eq!(old_prefix, "pf-v5-");
                assert_eq!(new_prefix, "pf-v6-");
                assert!(exclude_patterns.is_empty());
            }
            other => panic!("Expected CssVariablePrefix, got {:?}", other),
        }
    }

    #[test]
    fn test_css_variable_prefix_missing_fields_falls_to_manual() {
        let entry = make_strategy_entry("CssVariablePrefix");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_import_path_change() {
        let mut entry = make_strategy_entry("ImportPathChange");
        entry.from = Some("@patternfly/react-core/deprecated".to_string());
        entry.to = Some("@patternfly/react-core".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::ImportPathChange { old_path, new_path } => {
                assert_eq!(old_path, "@patternfly/react-core/deprecated");
                assert_eq!(new_path, "@patternfly/react-core");
            }
            other => panic!("Expected ImportPathChange, got {:?}", other),
        }
    }

    #[test]
    fn test_import_path_change_missing_fields_falls_to_manual() {
        let mut entry = make_strategy_entry("ImportPathChange");
        entry.from = Some("something".to_string());
        // missing `to`
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_ensure_dependency() {
        let mut entry = make_strategy_entry("EnsureDependency");
        entry.package = Some("@patternfly/react-core".to_string());
        entry.new_version = Some("^6.0.0".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::EnsureDependency {
                package,
                new_version,
            } => {
                assert_eq!(package, "@patternfly/react-core");
                assert_eq!(new_version, "^6.0.0");
            }
            other => panic!("Expected EnsureDependency, got {:?}", other),
        }
    }

    #[test]
    fn test_ensure_dependency_missing_fields_falls_to_manual() {
        let mut entry = make_strategy_entry("EnsureDependency");
        entry.package = Some("something".to_string());
        // missing new_version
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_remove_prop_maps_to_remove_attribute() {
        let entry = make_strategy_entry("RemoveProp");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::RemoveAttribute => {}
            other => panic!("Expected RemoveAttribute, got {:?}", other),
        }
    }

    #[test]
    fn test_prop_value_change_maps_to_llm() {
        let entry = make_strategy_entry("PropValueChange");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Llm { .. } => {}
            other => panic!("Expected Llm, got {:?}", other),
        }
    }

    #[test]
    fn test_unknown_strategy_maps_to_manual() {
        let entry = make_strategy_entry("SomethingUnknown");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_fix_plan_default_is_empty() {
        let plan = FixPlan::default();
        assert!(plan.files.is_empty());
        assert!(plan.manual.is_empty());
        assert!(plan.pending_llm.is_empty());
    }

    #[test]
    fn test_fix_result_default_is_zero() {
        let result = FixResult::default();
        assert_eq!(result.files_modified, 0);
        assert_eq!(result.edits_applied, 0);
        assert_eq!(result.edits_skipped, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_fix_confidence_serde() {
        assert_eq!(
            serde_json::to_string(&FixConfidence::Exact).unwrap(),
            "\"exact\""
        );
        assert_eq!(
            serde_json::to_string(&FixConfidence::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::to_string(&FixConfidence::Medium).unwrap(),
            "\"medium\""
        );
        assert_eq!(
            serde_json::to_string(&FixConfidence::Low).unwrap(),
            "\"low\""
        );
    }

    #[test]
    fn test_fix_source_serde() {
        assert_eq!(
            serde_json::to_string(&FixSource::Pattern).unwrap(),
            "\"pattern\""
        );
        assert_eq!(serde_json::to_string(&FixSource::Llm).unwrap(), "\"llm\"");
        assert_eq!(
            serde_json::to_string(&FixSource::Manual).unwrap(),
            "\"manual\""
        );
    }

    #[test]
    fn test_strategy_entry_json_deserialization() {
        let json = r#"{
            "strategy": "Rename",
            "mappings": [
                {"from": "Chip", "to": "Label"},
                {"from": "ChipGroup", "to": "LabelGroup"}
            ]
        }"#;
        let entry: FixStrategyEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.strategy, "Rename");
        assert_eq!(entry.mappings.len(), 2);
        assert_eq!(entry.mappings[0].from.as_deref(), Some("Chip"));
        assert_eq!(entry.mappings[0].to.as_deref(), Some("Label"));
    }
}
