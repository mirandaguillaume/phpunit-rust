use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

use phpunit_rust::discovery::{discover_in_dir, discover_in_dirs};

/// Parse composer.json's `autoload-dev.psr-4` and `autoload-dev.classmap`
/// entries into a list of directories, resolved relative to `project`.
/// Returns an empty Vec if the file is absent or has no autoload-dev.
fn parse_autoload_dev_dirs(project: &std::path::Path) -> Vec<PathBuf> {
    let path = project.join("composer.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return vec![]; };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![]; };
    let Some(dev) = val.get("autoload-dev") else { return vec![]; };
    let mut dirs = Vec::new();
    // psr-4: { "Ns\\" : "src/" }  — values can be a string or an array of strings
    if let Some(psr4) = dev.get("psr-4").and_then(|v| v.as_object()) {
        for v in psr4.values() {
            match v {
                serde_json::Value::String(s) => {
                    let p = project.join(s);
                    if p.is_dir() { dirs.push(p); }
                }
                serde_json::Value::Array(arr) => {
                    for s in arr {
                        if let Some(s) = s.as_str() {
                            let p = project.join(s);
                            if p.is_dir() { dirs.push(p); }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // classmap: ["tests/", ...]
    if let Some(arr) = dev.get("classmap").and_then(|v| v.as_array()) {
        for s in arr {
            if let Some(s) = s.as_str() {
                let p = project.join(s);
                if p.is_dir() { dirs.push(p); }
            }
        }
    }
    dirs
}
use phpunit_rust::php_worker::{check_php_version, find_worker_script, PhpWorkerPool};
use phpunit_rust::phpunit_xml::{parse_bootstrap, parse_php_constants, parse_testsuites};
use phpunit_rust::reporter::{print_progress, print_summary};
use phpunit_rust::runner::{run, RunConfig};

#[derive(Parser, Debug)]
#[command(name = "phpunit-rust", version, about = "PHPUnit-compatible test runner")]
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
    /// Path to phpunit.xml. Defaults to <project>/phpunit.xml or
    /// phpunit.xml.dist if found. We extract: the `bootstrap` attribute,
    /// `<testsuite><directory>` entries (used as additional discovery roots),
    /// and `<php><const>` declarations (passed to the worker as `define()`s).
    #[arg(long)]
    configuration: Option<PathBuf>,
    /// Number of parallel PHP workers. Defaults to the number of CPU
    /// cores detected on this machine. Use --workers 1 for sequential mode.
    #[arg(long)]
    workers: Option<usize>,
    /// Minimum row count for a data-provider method to be split into per-row
    /// chunks. Below this, methods are dispatched whole. Default 50.
    #[arg(long)]
    row_chunk_min: Option<usize>,
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

    // Read the phpunit.xml once; reuse for bootstrap + testsuites + constants.
    let xml_str = match &xml_path {
        Some(xml) => Some(
            std::fs::read_to_string(xml)
                .with_context(|| format!("reading {}", xml.display()))?,
        ),
        None => None,
    };

    let bootstrap = match (cli.bootstrap, xml_str.as_deref()) {
        (Some(b), _) => Some(if b.is_absolute() { b } else { project.join(b) }),
        (None, Some(xml)) => parse_bootstrap(xml).map(|rel| {
            let p = PathBuf::from(&rel);
            if p.is_absolute() { p } else { project.join(p) }
        }),
        (None, None) => None,
    };
    if let Some(b) = &bootstrap {
        eprintln!("Using bootstrap: {}", b.display());
    }

    // <php><const>: forwarded to the worker so PHP's `define()` runs them
    // before tests do.
    let defines: Vec<[String; 2]> = xml_str
        .as_deref()
        .map(parse_php_constants)
        .unwrap_or_default()
        .into_iter()
        .map(|c| [c.name, c.value])
        .collect();
    if !defines.is_empty() {
        eprintln!("Applying {} <php><const> declaration{} from configuration.",
            defines.len(),
            if defines.len() == 1 { "" } else { "s" });
    }

    // <testsuites>: collect include directories + excludes, resolved relative
    // to the project root. If phpunit.xml declares testsuites we use them as
    // the discovery roots; otherwise we fall back to --tests-dir.
    let (test_roots, excludes): (Vec<PathBuf>, Vec<PathBuf>) = match xml_str.as_deref() {
        Some(xml) => {
            let suites = parse_testsuites(xml);
            if suites.is_empty() {
                let dir = project.join(&cli.tests_dir);
                (vec![dir], vec![])
            } else {
                let mut roots = Vec::new();
                let mut excls = Vec::new();
                for s in suites {
                    for d in s.directories {
                        let p = PathBuf::from(&d);
                        roots.push(if p.is_absolute() { p } else { project.join(p) });
                    }
                    for d in s.excludes {
                        let p = PathBuf::from(&d);
                        excls.push(if p.is_absolute() { p } else { project.join(p) });
                    }
                }
                (roots, excls)
            }
        }
        None => {
            let dir = project.join(&cli.tests_dir);
            (vec![dir], vec![])
        }
    };

    // Validate discovery roots exist; warn (not error) if some don't, since
    // phpunit.xml may reference optional or conditionally-installed suites.
    let test_roots: Vec<PathBuf> = test_roots
        .into_iter()
        .filter(|p| {
            if p.is_dir() {
                true
            } else {
                eprintln!("warning: test directory not found, skipping: {}", p.display());
                false
            }
        })
        .collect();
    if test_roots.is_empty() {
        return Err(anyhow!(
            "no discoverable test directories — checked --tests-dir and phpunit.xml's <testsuites>"
        ));
    }

    let worker_count = cli.workers.unwrap_or_else(num_cpus::get).max(1);
    rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .build_global()
        .context("initializing rayon thread pool")?;

    // Read composer.json autoload-dev dirs to enrich the class graph with
    // abstract base classes that live outside the phpunit.xml testsuite dirs.
    let graph_supplement_dirs = parse_autoload_dev_dirs(&project);

    if test_roots.len() == 1 {
        eprintln!("Discovering tests in {}...", test_roots[0].display());
    } else {
        eprintln!("Discovering tests across {} roots ({} excludes)...",
            test_roots.len(), excludes.len());
    }
    let cases = if test_roots.len() == 1 && excludes.is_empty() && graph_supplement_dirs.is_empty() {
        discover_in_dir(&test_roots[0])?
    } else {
        discover_in_dirs(&test_roots, &excludes, &graph_supplement_dirs)?
    };
    eprintln!("Found {} test methods across {} classes.",
        cases.len(),
        cases.iter().map(|c| &c.class).collect::<std::collections::BTreeSet<_>>().len()
    );

    // Verify a usable PHP is on PATH. We require ≥ 8.1; some projects need
    // newer (brick/math: 8.2; doctrine/collections: 8.4). The user is on
    // the hook for installing a sufficiently-new PHP.
    let php_id = check_php_version(80100)
        .context("PHP version check failed (need ≥ 8.1 on PATH)")?;
    eprintln!("PHP version id: {php_id}");

    eprintln!("Spawning {} PHP worker{}...", worker_count, if worker_count == 1 { "" } else { "s" });
    let worker_script = find_worker_script()?;
    let pool = PhpWorkerPool::spawn(&worker_script, worker_count)?;

    let cfg = RunConfig {
        autoload,
        bootstrap,
        filter: cli.filter,
        defines,
        row_chunk_min: cli.row_chunk_min.unwrap_or(50),
    };
    let report = run(&pool, cases, &cfg, |o| print_progress(o))?;
    print_summary(&report);

    if report.is_success() { Ok(ExitCode::SUCCESS) } else { Ok(ExitCode::from(1)) }
}
