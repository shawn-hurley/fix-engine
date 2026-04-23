pub mod fix;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};

#[derive(Parser)]
#[command(
    name = "fix-engine",
    about = "Generic fix engine for applying pattern-based and LLM-assisted code migration fixes",
    version,
    after_help = "ENVIRONMENT VARIABLES:\n  \
        RUST_LOG    Control log verbosity (e.g., RUST_LOG=debug). Default: info.\n                \
        Log output is routed through the progress display so it won't\n                \
        clobber active spinners or progress bars."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Apply fixes based on Konveyor analysis output.
    Fix(fix::FixOpts),

    /// Generate shell completions for the given shell.
    Completions {
        /// Shell to generate completions for.
        shell: Shell,
    },
}

/// Print shell completions to stdout.
pub fn print_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "fix-engine", &mut std::io::stdout());
}
