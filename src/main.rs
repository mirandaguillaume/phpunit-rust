use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

use phpunit_rust::discovery::discover_in_dir;
use phpunit_rust::frankenphp::{find_worker_script, WorkerPool};
use phpunit_rust::phpunit_xml::parse_bootstrap;
use phpunit_rust::reporter::{print_progress, print_summary};
use phpunit_rust::runner::{run, RunConfig};

#[derive(Parser, Debug)]
#[command(name = "phpunit-rust", version, about = "PHPUnit-compatible test runner via FrankenPHP")]
struct Cli {
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[arg(long, default_value = "tests")]
    tests_dir: PathBuf,
    #[arg(long)]
    filter: Option<String>,
    /// Bootstrap file to require before any tests. Overrides phpunit.xml's
    /// <bootstrap> attribute if both are present.
    #[arg(long)]
    bootstrap: Option<PathBuf>,
    /// Path to phpunit.xml (only used to extract its `bootstrap` attribute).
    /// Defaults to <project>/phpunit.xml or phpunit.xml.dist if found.
    #[arg(long)]
    configuration: Option<PathBuf>,
    /// Number of parallel FrankenPHP workers. Defaults to the number of CPU
    /// cores detected on this machine. Use --workers 1 for sequential mode.
    #[arg(long)]
    workers: Option<usize>,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn real_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let project = cli.project.canonicalize()
        .with_context(|| format!("project path invalid: {}", cli.project.display()))?;
    let autoload = project.join("vendor/autoload.php");
    if !autoload.is_file() {
        return Err(anyhow!(
            "autoload not found at {}; run `composer install` first",
            autoload.display()
        ));
    }
    let tests_dir = project.join(&cli.tests_dir);
    if !tests_dir.is_dir() {
        return Err(anyhow!("tests directory not found: {}", tests_dir.display()));
    }

    let xml_path = match cli.configuration {
        Some(p) => Some(if p.is_absolute() { p } else { project.join(p) }),
        None => {
            let auto = project.join("phpunit.xml");
            if auto.is_file() {
                Some(auto)
            } else {
                let dist = project.join("phpunit.xml.dist");
                if dist.is_file() { Some(dist) } else { None }
            }
        }
    };
    let bootstrap = match (cli.bootstrap, xml_path) {
        (Some(b), _) => Some(if b.is_absolute() { b } else { project.join(b) }),
        (None, Some(xml)) => {
            let xml_str = std::fs::read_to_string(&xml)
                .with_context(|| format!("reading {}", xml.display()))?;
            parse_bootstrap(&xml_str).map(|rel| {
                let p = PathBuf::from(&rel);
                if p.is_absolute() { p } else { project.join(p) }
            })
        }
        (None, None) => None,
    };
    if let Some(b) = &bootstrap {
        eprintln!("Using bootstrap: {}", b.display());
    }

    // Decide worker count BEFORE initializing rayon. We need the rayon pool
    // sized to match so `rayon::current_thread_index()` returns valid indices
    // into our WorkerClient vec.
    let worker_count = cli.workers.unwrap_or_else(num_cpus::get).max(1);
    rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .build_global()
        .context("initializing rayon thread pool")?;

    eprintln!("Discovering tests in {}...", tests_dir.display());
    let cases = discover_in_dir(&tests_dir)?;
    eprintln!("Found {} test methods across {} classes.",
        cases.len(),
        cases.iter().map(|c| &c.class).collect::<std::collections::BTreeSet<_>>().len()
    );

    eprintln!("Spawning {} FrankenPHP worker{}...", worker_count, if worker_count == 1 { "" } else { "s" });
    let worker_script = find_worker_script()?;
    let pool = WorkerPool::spawn(&worker_script, worker_count)?;

    let cfg = RunConfig { autoload, bootstrap, filter: cli.filter };
    let report = run(&pool, cases, &cfg, |o| print_progress(o))?;
    print_summary(&report);

    if report.is_success() { Ok(ExitCode::SUCCESS) } else { Ok(ExitCode::from(1)) }
}
