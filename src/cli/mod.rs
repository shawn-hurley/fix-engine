pub mod fix;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "fix-engine",
    about = "Generic fix engine for applying pattern-based and LLM-assisted code migration fixes",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Apply fixes based on Konveyor analysis output.
    Fix(fix::FixOpts),
}
