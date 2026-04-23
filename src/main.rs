use anyhow::Result;
use clap::Parser;

mod cli;
pub mod progress;

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let args = cli::Cli::parse();

    match args.command {
        cli::Command::Fix(opts) => {
            // Configure colored output based on --color flag.
            match opts.color {
                cli::fix::ColorMode::Always => {
                    owo_colors::set_override(true);
                }
                cli::fix::ColorMode::Never => {
                    owo_colors::set_override(false);
                }
                cli::fix::ColorMode::Auto => {
                    // owo-colors auto-detects by default; nothing to do.
                }
            }

            // Create the progress reporter (quiet mode suppresses spinners/bars).
            let reporter = progress::ProgressReporter::new(opts.quiet);

            // Wire tracing through the IndicatifWriter so log lines don't
            // clobber active progress bars.
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .with_writer(reporter.make_writer())
                .init();

            cli::fix::run(opts, &reporter).await
        }
        cli::Command::Completions { shell } => {
            cli::print_completions(shell);
            Ok(())
        }
    }
}
