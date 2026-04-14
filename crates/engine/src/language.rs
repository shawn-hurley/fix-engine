//! Language-specific fix provider trait.
//!
//! The fix engine is language-agnostic -- it works with any Konveyor analysis
//! output regardless of the source language. However, certain fix operations
//! (attribute removal, import deduplication, path skipping, dependency
//! management) require knowledge of the target language's syntax and ecosystem.
//!
//! This module defines the [`LanguageFixProvider`] trait that language-specific
//! crates implement, plus a [`NoOpLanguageFixProvider`] fallback that performs
//! no language-specific processing.

use fix_engine_core::{PlannedFix, RenameMapping};
use konveyor_core::incident::Incident;
use std::path::Path;

/// Trait that language-specific crates implement to provide syntax-aware
/// fix operations for the fix engine.
///
/// Implementations are passed to [`plan_fixes`](crate::engine::plan_fixes),
/// [`apply_fixes`](crate::engine::apply_fixes), and
/// [`preview_fixes`](crate::engine::preview_fixes) at runtime.
pub trait LanguageFixProvider: Send + Sync {
    /// Should this file path be skipped during fix planning?
    ///
    /// For example, a JS/TS provider skips `node_modules/` since those
    /// dependencies are updated via package manager, not source patches.
    fn should_skip_path(&self, path: &Path) -> bool;

    /// Post-process lines after edits have been applied.
    ///
    /// Called once per file after all text edits are applied. Implementations
    /// can use this to clean up language-specific artifacts (e.g., deduplicating
    /// import specifiers after renames produce duplicates).
    fn post_process_lines(&self, lines: &mut [String]);

    /// Plan an attribute/prop removal fix.
    ///
    /// Given an incident flagging an attribute for removal, produce a
    /// [`PlannedFix`] with the text edits needed to remove it. Returns `None`
    /// if the incident cannot be processed.
    fn plan_remove_attribute(
        &self,
        rule_id: &str,
        incident: &Incident,
        file_path: &Path,
    ) -> Option<PlannedFix>;

    /// Plan a dependency version fix.
    ///
    /// Given an incident requiring a dependency to be at a specific version,
    /// produce a [`PlannedFix`] with the text edits needed to update or add
    /// the dependency in the appropriate manifest file (e.g., `package.json`
    /// for Node.js, `Cargo.toml` for Rust, `go.mod` for Go).
    ///
    /// Returns `None` if the language provider does not support dependency
    /// management or if the incident cannot be processed.
    fn plan_ensure_dependency(
        &self,
        rule_id: &str,
        incident: &Incident,
        package: &str,
        new_version: &str,
        file_path: &Path,
    ) -> Option<PlannedFix>;

    /// Extract the matched text from incident variables.
    ///
    /// Incidents carry language-specific variable names (e.g., `propName`,
    /// `className`, `variableName`). This method extracts the primary matched
    /// text from whichever variable is present.
    fn get_matched_text(&self, incident: &Incident) -> String;

    /// Get the matched text for rename operations.
    ///
    /// Rename mappings may target either the attribute name or its value.
    /// This method inspects incident variables to find which mapping entry
    /// matches, considering both names and values.
    fn get_matched_text_for_rename(
        &self,
        incident: &Incident,
        mappings: &[RenameMapping],
    ) -> String;

    /// Whether a rename incident requires whole-file scanning.
    ///
    /// Some renames (e.g., component/import renames in JSX) affect many lines
    /// beyond the incident line -- opening tags, closing tags, type references.
    /// When this returns `true`, the engine scans the entire file for all
    /// occurrences of the rename mappings.
    fn is_whole_file_rename(&self, incident: &Incident) -> bool;
}

/// No-op fallback provider for languages without specific fix support.
///
/// Skips no paths, performs no post-processing, and returns `None` / empty
/// defaults for all language-specific operations. The engine still applies
/// generic strategies (text replacement renames, import path changes, etc.).
pub struct NoOpLanguageFixProvider;

impl LanguageFixProvider for NoOpLanguageFixProvider {
    fn should_skip_path(&self, _path: &Path) -> bool {
        false
    }

    fn post_process_lines(&self, _lines: &mut [String]) {
        // No post-processing
    }

    fn plan_remove_attribute(
        &self,
        _rule_id: &str,
        _incident: &Incident,
        _file_path: &Path,
    ) -> Option<PlannedFix> {
        None
    }

    fn plan_ensure_dependency(
        &self,
        _rule_id: &str,
        _incident: &Incident,
        _package: &str,
        _new_version: &str,
        _file_path: &Path,
    ) -> Option<PlannedFix> {
        None
    }

    fn get_matched_text(&self, _incident: &Incident) -> String {
        String::new()
    }

    fn get_matched_text_for_rename(
        &self,
        _incident: &Incident,
        _mappings: &[RenameMapping],
    ) -> String {
        String::new()
    }

    fn is_whole_file_rename(&self, _incident: &Incident) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_provider_skips_nothing() {
        let provider = NoOpLanguageFixProvider;
        assert!(!provider.should_skip_path(Path::new("/some/path")));
        assert!(!provider.should_skip_path(Path::new("/node_modules/foo")));
    }

    #[test]
    fn test_noop_provider_no_post_processing() {
        let provider = NoOpLanguageFixProvider;
        let mut lines = vec!["import { Foo, Foo } from 'bar';".to_string()];
        provider.post_process_lines(&mut lines);
        // Lines should be unchanged
        assert_eq!(lines[0], "import { Foo, Foo } from 'bar';");
    }

    #[test]
    fn test_noop_provider_returns_none_for_remove() {
        let provider = NoOpLanguageFixProvider;
        let incident = konveyor_core::incident::Incident {
            file_uri: "file:///test.rs".to_string(),
            line_number: Some(1),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables: std::collections::BTreeMap::new(),
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        };
        assert!(provider
            .plan_remove_attribute("rule", &incident, Path::new("/test.rs"))
            .is_none());
    }

    #[test]
    fn test_noop_provider_returns_none_for_ensure_dependency() {
        let provider = NoOpLanguageFixProvider;
        let incident = konveyor_core::incident::Incident {
            file_uri: "file:///test.rs".to_string(),
            line_number: Some(1),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables: std::collections::BTreeMap::new(),
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        };
        assert!(provider
            .plan_ensure_dependency("rule", &incident, "pkg", "1.0.0", Path::new("/test.rs"))
            .is_none());
    }

    #[test]
    fn test_noop_provider_empty_matched_text() {
        let provider = NoOpLanguageFixProvider;
        let incident = konveyor_core::incident::Incident {
            file_uri: String::new(),
            line_number: Some(1),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables: std::collections::BTreeMap::new(),
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        };
        assert_eq!(provider.get_matched_text(&incident), "");
        assert_eq!(provider.get_matched_text_for_rename(&incident, &[]), "");
        assert!(!provider.is_whole_file_rename(&incident));
    }
}
