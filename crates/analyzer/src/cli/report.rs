//! `pcov-rs report` — risk-ranked method report (coverage × complexity).

use std::path::PathBuf;

use crate::boundary::BoundaryResolver;
use crate::cli::analyze;
use crate::complexity;
use crate::config::{self, ConfigError};
use crate::report;

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    pub common: super::CommonOpts,

    /// Minimum risk score to include (default: show all)
    #[arg(long, default_value = "0.0")]
    pub threshold: f64,

    /// Output format: table | json
    #[arg(long, default_value = "table")]
    pub format: String,

    /// Write output to file (default: stdout)
    #[arg(long)]
    pub output: Option<PathBuf>,
}

pub fn run(args: Args) -> anyhow::Result<()> {
    let cfg = match config::parse(&args.common.config) {
        Ok(c) => c,
        Err(ConfigError::NotFound(p)) => {
            anyhow::bail!("Couldn't find {p:?}. Pass --config or run from a project root.");
        }
        Err(e) => return Err(e.into()),
    };

    let boundary = BoundaryResolver::from_config(&cfg);

    // Reuse the full analysis pipeline (warm path: single result-cache read).
    let coverage = analyze::analyze_filtered(&cfg, None)?;

    // Load project for complexity extraction. Vendor-skip is fine: boundary
    // filtering already excludes vendor methods from the report.
    let project = crate::mago_bridge::MagoProject::load_excluding_vendor(&cfg.root)?;

    let complexity = complexity::compute_all(&project, &boundary);
    let report = report::build(&coverage, &complexity);

    let rendered = match args.format.as_str() {
        "json" => report::render_json(&report, args.threshold),
        _ => report::render_table(&report, args.threshold),
    };

    if let Some(path) = args.output {
        std::fs::write(path, rendered)?;
    } else {
        print!("{rendered}");
    }
    Ok(())
}
