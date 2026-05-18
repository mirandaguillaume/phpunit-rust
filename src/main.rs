use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

use phpunit_rust::client::WorkerClient;
use phpunit_rust::discovery::discover_in_dir;
use phpunit_rust::frankenphp::{find_worker_script, FrankenPhp};
use phpunit_rust::reporter::{print_progress, print_summary};
use phpunit_rust::runner::{run, RunConfig};

#[derive(Parser, Debug)]
#[command(name = "phpunit-rust", version, about = "PHPUnit-compatible test runner via FrankenPHP")]
struct Cli {
    /// Path to the project under test (must contain composer.json + vendor/).
    #[arg(long, default_value = ".")]
    project: PathBuf,

    /// Subdirectory (relative to --project) containing test files.
    #[arg(long, default_value = "tests")]
    tests_dir: PathBuf,

    /// Run only tests whose `Class::method` contains this substring.
    #[arg(long)]
    filter: Option<String>,

    /// Path to phpunit.xml. Defaults to <project>/phpunit.xml if it exists.
    #[arg(long)]
    configuration: Option<PathBuf>,
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

    // Auto-detect phpunit.xml if --configuration wasn't supplied.
    let phpunit_xml = match cli.configuration {
        Some(p) => {
            let abs = if p.is_absolute() { p } else { project.join(p) };
            Some(abs.canonicalize().context("invalid --configuration path")?)
        }
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
    if let Some(p) = &phpunit_xml {
        eprintln!("Using configuration: {}", p.display());
    }

    eprintln!("Discovering tests in {}...", tests_dir.display());
    let cases = discover_in_dir(&tests_dir)?;
    eprintln!("Found {} test methods across {} classes.",
        cases.len(),
        cases.iter().map(|c| &c.class).collect::<std::collections::BTreeSet<_>>().len()
    );

    let worker = find_worker_script()?;
    let fph = FrankenPhp::spawn(&worker)?;
    let client = WorkerClient::new(fph.worker_url());

    let cfg = RunConfig { autoload, phpunit_xml, filter: cli.filter };
    let report = run(&client, cases, &cfg, |o| print_progress(o))?;
    print_summary(&report);

    if report.is_success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}
