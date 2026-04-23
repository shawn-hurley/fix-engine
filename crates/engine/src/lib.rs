//! Fix engine for applying pattern-based and LLM-assisted code migration fixes.
//!
//! This crate provides the core fix planning and application logic:
//! - `language`: trait for language-specific fix operations
//! - `context`: trait for framework-specific LLM prompt customization
//! - `registry`: registry of FixContext implementations
//! - `engine`: pattern-based fix planning/applying (rename, removal, path change, etc.)
//! - `llm_client`: OpenAI-compatible LLM client for AI-assisted fixes
//! - `goose_client`: goose CLI subprocess client for AI-assisted fixes

pub mod context;
pub mod engine;
pub mod goose_client;
pub mod language;
pub mod llm_client;
pub mod progress;
pub mod registry;
