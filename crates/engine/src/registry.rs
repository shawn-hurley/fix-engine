//! Registry of [`FixContext`] implementations, keyed by ruleset name.
//!
//! The registry maps Konveyor ruleset names to framework-specific LLM
//! prompt contexts. When the fix engine encounters a ruleset, it looks
//! up the matching context. If none is found, a generic fallback is used.

use crate::context::{FixContext, GenericFixContext};
use std::collections::HashMap;

/// Registry that maps ruleset names to their [`FixContext`] implementations.
pub struct FixContextRegistry {
    contexts: HashMap<String, Box<dyn FixContext>>,
    fallback: GenericFixContext,
}

impl FixContextRegistry {
    /// Create a new empty registry with the generic fallback.
    pub fn new() -> Self {
        Self {
            contexts: HashMap::new(),
            fallback: GenericFixContext,
        }
    }

    /// Register a [`FixContext`] implementation.
    /// The context is keyed by its `ruleset_name()`.
    pub fn register(&mut self, ctx: Box<dyn FixContext>) {
        let name = ctx.ruleset_name().to_string();
        self.contexts.insert(name, ctx);
    }

    /// Look up the [`FixContext`] for a given ruleset name.
    /// Returns the generic fallback if no match is found.
    pub fn get(&self, ruleset_name: &str) -> &dyn FixContext {
        self.contexts
            .get(ruleset_name)
            .map(|b| b.as_ref())
            .unwrap_or(&self.fallback)
    }

    /// Returns true if a context is registered for the given ruleset name.
    pub fn has(&self, ruleset_name: &str) -> bool {
        self.contexts.contains_key(ruleset_name)
    }
}

impl Default for FixContextRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestContext {
        name: String,
    }

    impl FixContext for TestContext {
        fn ruleset_name(&self) -> &str {
            &self.name
        }

        fn migration_description(&self) -> &str {
            "test migration"
        }

        fn llm_constraints(&self) -> &[String] {
            &[]
        }
    }

    #[test]
    fn test_registry_fallback() {
        let registry = FixContextRegistry::new();
        let ctx = registry.get("nonexistent");
        assert_eq!(ctx.migration_description(), "code migration");
    }

    #[test]
    fn test_registry_register_and_lookup() {
        let mut registry = FixContextRegistry::new();
        registry.register(Box::new(TestContext {
            name: "test-rules".to_string(),
        }));

        let ctx = registry.get("test-rules");
        assert_eq!(ctx.migration_description(), "test migration");
        assert!(registry.has("test-rules"));
        assert!(!registry.has("other-rules"));
    }

    #[test]
    fn test_registry_unknown_returns_fallback() {
        let mut registry = FixContextRegistry::new();
        registry.register(Box::new(TestContext {
            name: "test-rules".to_string(),
        }));

        let ctx = registry.get("unknown-rules");
        assert_eq!(ctx.migration_description(), "code migration");
    }
}
