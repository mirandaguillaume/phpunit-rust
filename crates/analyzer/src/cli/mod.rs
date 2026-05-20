//! CLI subcommand definitions and routing.

pub mod analyze;
pub mod report;
pub mod test_discovery;
pub mod cache_cmd;

use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum Command {
    /// Run coverage analysis and emit results
    Analyze(analyze::Args),
    /// Refresh test discovery cache (does not produce coverage)
    TestDiscovery(test_discovery::Args),
    /// Manage the on-disk cache
    Cache(cache_cmd::Args),
    /// Risk-ranked method report (coverage × cyclomatic complexity)
    Report(report::Args),
}

#[derive(clap::Args, Clone)]
pub struct CommonOpts {
    /// Path to phpunit.xml
    #[arg(long, default_value = "./phpunit.xml")]
    pub config: PathBuf,

    /// Override test paths (repeatable)
    #[arg(long)]
    pub tests: Vec<PathBuf>,

    /// Override source paths (repeatable)
    #[arg(long)]
    pub source: Vec<PathBuf>,

    /// Emit warnings for opaque/skipped constructs
    #[arg(long)]
    pub verbose: bool,
}
