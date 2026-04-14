//! Fix context trait for framework/ruleset-specific LLM prompt customization.
//!
//! The fix engine is generic — it works with any Konveyor analysis output.
//! But LLM-assisted fixes benefit from framework-specific guidance (prompt
//! constraints, priority ordering, system prompts). This module defines the
//! trait that framework crates implement, plus a generic fallback.

/// Trait that framework/ruleset-specific crates implement to provide
/// LLM prompt context for the fix engine.
///
/// Implementations are registered in a [`FixContextRegistry`](super::registry::FixContextRegistry)
/// and looked up by ruleset name at runtime.
pub trait FixContext: Send + Sync {
    /// Unique name that matches the `RuleSet.name` field from Konveyor
    /// analysis output. Used for registry lookup.
    fn ruleset_name(&self) -> &str;

    /// Human-readable description of the migration (e.g., "PatternFly v5 to v6").
    /// Used in LLM system prompts and prompt preambles.
    fn migration_description(&self) -> &str;

    /// Framework-specific constraints to include in LLM prompts.
    /// Each string is a single constraint line (will be prefixed with "- ").
    fn llm_constraints(&self) -> &[String];

    /// Warning text about revert pitfalls for batch mode context sections.
    /// Shown when a batch prompt includes previously-applied fixes to warn
    /// the LLM about framework-specific patterns it should not undo.
    ///
    /// Returns `None` if no revert warnings are needed.
    fn revert_warnings(&self) -> Option<&str> {
        None
    }

    /// Determine the processing priority of a fix request within a batch.
    /// Lower number = processed first. This ensures structural migration
    /// rules come before informational/review-only rules.
    ///
    /// Default: all rules have equal priority (3).
    fn fix_priority(&self, _rule_id: &str) -> u8 {
        3
    }

    /// Examples of change types for LLM prompt context.
    ///
    /// Used in batch prompts to describe the kinds of changes the LLM should
    /// look for. Should be a parenthetical list of language-specific actions.
    ///
    /// Default: generic description suitable for any language.
    fn change_type_examples(&self) -> &str {
        "add/remove/rename identifiers, update references"
    }

    /// Optional verification instructions appended to the LLM batch prompt.
    ///
    /// Framework-specific checks the LLM should perform after applying fixes.
    /// Returns `None` if no verification instructions are needed.
    fn verification_prompt(&self) -> Option<&str> {
        None
    }

    /// System prompt for the OpenAI-compatible LLM client.
    ///
    /// Default implementation builds a generic prompt from `migration_description()`.
    fn llm_system_prompt(&self) -> String {
        format!(
            "You are a {} assistant. \
             Given a code snippet and a migration message, output ONLY the corrected \
             code for the affected lines. Output in this exact format:\n\n\
             ```fix\n\
             LINE:<line_number>\n\
             OLD:<exact old text on that line>\n\
             NEW:<replacement text>\n\
             ```\n\n\
             You may output multiple fix blocks. Do not include any explanation outside \
             the fix blocks. Only output fixes for lines that need to change.",
            self.migration_description()
        )
    }
}

/// Generic fallback context with no framework-specific guidance.
///
/// Used when no registered `FixContext` matches the ruleset name.
/// Provides reasonable defaults that work for any Konveyor analysis output.
pub struct GenericFixContext;

impl FixContext for GenericFixContext {
    fn ruleset_name(&self) -> &str {
        ""
    }

    fn migration_description(&self) -> &str {
        "code migration"
    }

    fn llm_constraints(&self) -> &[String] {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generic_context_defaults() {
        let ctx = GenericFixContext;
        assert_eq!(ctx.ruleset_name(), "");
        assert_eq!(ctx.migration_description(), "code migration");
        assert!(ctx.llm_constraints().is_empty());
        assert!(ctx.revert_warnings().is_none());
        assert_eq!(ctx.fix_priority("any-rule"), 3);
    }

    #[test]
    fn test_generic_context_system_prompt() {
        let ctx = GenericFixContext;
        let prompt = ctx.llm_system_prompt();
        assert!(prompt.contains("code migration"));
        assert!(prompt.contains("```fix"));
        assert!(prompt.contains("LINE:"));
    }
}
