use clap::Parser;
use analyzer::cli;

#[derive(Parser)]
#[command(name = "pcov-rs", version, about = "Fast PHP test coverage via static analysis")]
struct Cli {
    #[command(subcommand)]
    command: cli::Command,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Pick the tracing filter: --verbose forces at least WARN level so the user
    // sees opaque-construct warnings. Otherwise respect RUST_LOG (default: off).
    let verbose = match &cli.command {
        cli::Command::Analyze(a) => a.common.verbose,
        cli::Command::TestDiscovery(t) => t.common.verbose,
        cli::Command::Cache(_) | cli::Command::Report(_) => false,
    };

    let filter = if verbose {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"))
    } else {
        tracing_subscriber::EnvFilter::from_default_env()
    };

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    match cli.command {
        cli::Command::Analyze(args) => cli::analyze::run(args),
        cli::Command::TestDiscovery(args) => cli::test_discovery::run(args),
        cli::Command::Cache(args) => cli::cache_cmd::run(args),
        cli::Command::Report(args) => cli::report::run(args),
    }
}
