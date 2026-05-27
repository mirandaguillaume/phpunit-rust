use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

use phpunit_rust::discovery::{discover_in_dir, discover_in_dirs};

/// Parse composer.json's `autoload-dev` AND `autoload` PSR-4/classmap entries
/// into a list of directories, resolved relative to `project`. Used to build
/// a complete class graph for inheritance resolution (e.g. abstract base classes
/// in the main autoload like MockeryTestCase). Returns empty Vec if absent.
fn parse_autoload_dev_dirs(project: &std::path::Path) -> Vec<PathBuf> {
    let path = project.join("composer.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return vec![]; };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![]; };

    let mut dirs = Vec::new();
    for section in ["autoload-dev", "autoload"] {
        let Some(block) = val.get(section) else { continue };
        collect_psr4_dirs(block, project, &mut dirs);
        collect_classmap_dirs(block, project, &mut dirs);
    }
    dirs
}

fn collect_psr4_dirs(block: &serde_json::Value, project: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Some(psr4) = block.get("psr-4").and_then(|v| v.as_object()) else { return };
    for v in psr4.values() {
        match v {
            serde_json::Value::String(s) => {
                let p = project.join(s);
                if p.is_dir() { out.push(p); }
            }
            serde_json::Value::Array(arr) => {
                for s in arr {
                    if let Some(s) = s.as_str() {
                        let p = project.join(s);
                        if p.is_dir() { out.push(p); }
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_classmap_dirs(block: &serde_json::Value, project: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Some(arr) = block.get("classmap").and_then(|v| v.as_array()) else { return };
    for s in arr {
        if let Some(s) = s.as_str() {
            let p = project.join(s);
            if p.is_dir() { out.push(p); }
        }
    }
}
use phpunit_rust::fork_pool::PhpForkPool;
use phpunit_rust::php_worker::{check_php_version, find_enumerate_script, find_fork_script};
use phpunit_rust::provider_enum::{collect_provider_pairs, enumerate, RowCounts};
use phpunit_rust::phpunit_xml::{parse_bootstrap, parse_excluded_groups, parse_listeners, parse_php_block, parse_testsuites};
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
    /// Run only tests whose effective groups include any of these names.
    /// Comma-separated or repeated. PHPUnit-compatible.
    #[arg(long = "group", value_delimiter = ',')]
    groups: Vec<String>,
    /// Skip tests whose effective groups include any of these names.
    /// Adds to phpunit.xml's <groups><exclude>. Comma-separated or repeated.
    #[arg(long = "exclude-group", value_delimiter = ',')]
    exclude_groups: Vec<String>,
    /// Run only the named testsuite from phpunit.xml (matches the `name`
    /// attribute of a <testsuite> element). If absent, all suites run.
    #[arg(long)]
    testsuite: Option<String>,
    /// Stop dispatching new tests after the first failed test
    /// (failed / errored). Tests already in flight on other workers
    /// still run to completion.
    #[arg(long)]
    stop_on_failure: bool,
    /// Like --stop-on-failure but also stops on skipped / incomplete /
    /// risky outcomes. Convenience for "stop on anything not-pass".
    #[arg(long)]
    stop_on_defect: bool,
    /// Print the full list of (class, method) tests that would be run
    /// for the current config (after group filtering and testsuite
    /// selection), then exit without running anything. Matches vanilla's
    /// --list-tests format.
    #[arg(long)]
    list_tests: bool,
    /// Rewrite `createMock()` patterns in test files into anonymous-class stubs
    /// before execution. Requires that mocked interfaces are resolvable via
    /// the project's PSR-4 autoload map in composer.json.
    #[arg(long)]
    bake_mocks: bool,
    /// Emit static coverage after the test run. Requires the `coverage` Cargo feature.
    /// Formats: clover | json | pcov | pcov-extended
    #[cfg(feature = "coverage")]
    #[arg(long)]
    coverage_format: Option<String>,
    /// Write coverage output to this file (default: stdout).
    #[cfg(feature = "coverage")]
    #[arg(long)]
    coverage_out: Option<std::path::PathBuf>,
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

    // <php> block: const + env + server + ini, all in one walk over
    // the XML. Forwarded to the master so it can apply them once
    // before fork (so each child inherits the configured state via COW).
    let php_block = xml_str.as_deref()
        .map(parse_php_block)
        .unwrap_or_default();
    let defines: Vec<[String; 2]> = php_block.constants.iter()
        .map(|c| [c.name.clone(), c.value.clone()])
        .collect();
    let env_triples: Vec<(String, String, bool)> = php_block.env.iter()
        .map(|e| (e.name.clone(), e.value.clone(), e.force))
        .collect();
    let server_pairs: Vec<[String; 2]> = php_block.server.iter()
        .map(|s| [s.name.clone(), s.value.clone()])
        .collect();
    let ini_pairs: Vec<[String; 2]> = php_block.ini.iter()
        .map(|s| [s.name.clone(), s.value.clone()])
        .collect();
    let total = defines.len() + env_triples.len() + server_pairs.len() + ini_pairs.len();
    if total > 0 {
        eprintln!("Applying <php> block: {} const, {} env, {} server, {} ini",
            defines.len(), env_triples.len(), server_pairs.len(), ini_pairs.len());
    }

    // <testsuites>: collect include directories + excludes, resolved relative
    // to the project root. If phpunit.xml declares testsuites we use them as
    // the discovery roots; otherwise we fall back to --tests-dir.
    let (test_roots, excludes): (Vec<PathBuf>, Vec<PathBuf>) = match xml_str.as_deref() {
        Some(xml) => {
            let mut suites = parse_testsuites(xml);
            // --testsuite NAME filters to a single suite (matches the
            // `name` attribute). Vanilla PHPUnit's --testsuite picks one.
            if let Some(target) = &cli.testsuite {
                let before = suites.len();
                suites.retain(|s| &s.name == target);
                if suites.is_empty() {
                    return Err(anyhow!(
                        "--testsuite '{}' did not match any of the {} <testsuite> entries in phpunit.xml",
                        target, before
                    ));
                }
                eprintln!("Selecting testsuite '{}' ({} suite{} matched)",
                    target, suites.len(), if suites.len() == 1 { "" } else { "s" });
            }
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
                // PHPUnit's <exclude> is per-testsuite — a directory excluded
                // by suite A but explicitly included by suite B should still
                // be walked. We flatten to a single (roots, excludes) pair
                // for discovery, so drop any exclude that also appears as a
                // root in another suite. (Guzzle-psr7's "tests/Integration"
                // hit this: suite 1 excludes it, suite 2 includes it.)
                let root_set: std::collections::HashSet<&PathBuf> = roots.iter().collect();
                excls.retain(|e| !root_set.contains(e));
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
    let mut cases = if test_roots.len() == 1 && excludes.is_empty() && graph_supplement_dirs.is_empty() {
        discover_in_dir(&test_roots[0])?
    } else {
        discover_in_dirs(&test_roots, &excludes, &graph_supplement_dirs)?
    };

    // Honor phpunit.xml's <groups><exclude>: drop any test whose effective
    // groups include one of the excluded names. Vanilla PHPUnit does this
    // at run time; we do it at discovery so the dispatch queue and the
    // outcome count match vanilla's.
    //
    // CLI flags --group / --exclude-group layer ON TOP:
    //   * --exclude-group adds names to the XML's exclude list.
    //   * --group restricts to tests whose groups intersect the include set
    //     (PHPUnit semantics: only the named groups run).
    let mut excluded_groups: Vec<String> = xml_str.as_deref()
        .map(parse_excluded_groups)
        .unwrap_or_default();
    for g in &cli.exclude_groups {
        if !excluded_groups.contains(g) { excluded_groups.push(g.clone()); }
    }
    if !excluded_groups.is_empty() {
        use std::collections::HashSet;
        let excl: HashSet<&str> = excluded_groups.iter().map(|s| s.as_str()).collect();
        let before = cases.len();
        cases.retain(|c| !c.groups.iter().any(|g| excl.contains(g.as_str())));
        let dropped = before - cases.len();
        eprintln!("Excluding {} test{} in groups: {}",
            dropped, if dropped == 1 { "" } else { "s" },
            excluded_groups.join(", "));
    }
    if !cli.groups.is_empty() {
        use std::collections::HashSet;
        let incl: HashSet<&str> = cli.groups.iter().map(|s| s.as_str()).collect();
        let before = cases.len();
        cases.retain(|c| c.groups.iter().any(|g| incl.contains(g.as_str())));
        eprintln!("Including only {} test{} in group{} {} (filtered {} → {})",
            cases.len(), if cases.len() == 1 { "" } else { "s" },
            if cli.groups.len() == 1 { "" } else { "s" },
            cli.groups.join(", "), before, cases.len());
    }

    // Symfony's PhpUnitTestsListener detection is intentionally NOT acted
    // upon: the listener's "SkippedTestCase wrapper" behaviour isn't
    // "every @group legacy test" — it inspects deprecation emissions at
    // run-time and conditionally skips. Replicating that without running
    // the listener itself is unsound (initial attempt over-skipped 526
    // cases when vanilla wraps only 14). Leaving this stub so we know
    // the detection is wired up if we later add generic <listeners>
    // dispatch.
    let _listeners: Vec<String> = xml_str.as_deref()
        .map(parse_listeners)
        .unwrap_or_default();
    let synthetic_legacy_skips: Vec<phpunit_rust::types::TestOutcome> = Vec::new();

    eprintln!("Found {} test methods across {} classes.",
        cases.len(),
        cases.iter().map(|c| &c.class).collect::<std::collections::BTreeSet<_>>().len()
    );

    // --bake-mocks: rewrite createMock patterns into anonymous classes before
    // dispatching. The temp_dir must outlive the pool so baked PHP files are
    // still on disk when the worker tries to require them.
    let _bake_temp_dir: Option<tempfile::TempDir>;
    if cli.bake_mocks {
        let td = tempfile::TempDir::new()
            .context("creating temp dir for baked test files")?;
        let rewritten = phpunit_rust::mock_bake::bake_test_cases(&cases, &project, &td);
        let baked_count = rewritten.iter().zip(cases.iter())
            .filter(|(a, b)| a.file != b.file)
            .count();
        if baked_count > 0 {
            eprintln!("Baked {baked_count} test file(s) (createMock → anonymous class).");
        }
        cases = rewritten;
        _bake_temp_dir = Some(td);
    } else {
        _bake_temp_dir = None;
    }

    // --list-tests: print "Class::method" lines (vanilla PHPUnit format)
    // and exit. We don't expand data-provider rows here — vanilla's own
    // --list-tests does, but that's a runtime side-effect; the static
    // list is the more useful CI primitive.
    if cli.list_tests {
        println!("Available tests:");
        for c in &cases {
            println!(" - {}::{}", c.class, c.method);
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Verify a usable PHP is on PATH. We require ≥ 8.1; some projects need
    // newer (brick/math: 8.2; doctrine/collections: 8.4). The user is on
    // the hook for installing a sufficiently-new PHP.
    let php_id = check_php_version(80100)
        .context("PHP version check failed (need ≥ 8.1 on PATH)")?;
    eprintln!("PHP version id: {php_id}");

    // Enumerate data-provider row counts BEFORE forking workers.
    // The runner uses these to decide whether to split a heavy provider
    // method into multiple stride-partitioned plans (see build_queue).
    // A failed enumeration is non-fatal: missing entries fall back to
    // single-bucket dispatch.
    let provider_pairs = collect_provider_pairs(&cases);
    let row_counts: RowCounts = if provider_pairs.is_empty() {
        RowCounts::new()
    } else {
        let enum_script = find_enumerate_script()?;
        match enumerate(&enum_script, &autoload, bootstrap.as_deref(), &defines, &provider_pairs) {
            Ok(counts) => counts,
            Err(e) => {
                eprintln!("Provider enumeration failed (continuing with no row data): {e:#}");
                RowCounts::new()
            }
        }
    };

    eprintln!("Spawning {} PHP worker{}...", worker_count, if worker_count == 1 { "" } else { "s" });
    let fork_script = find_fork_script()?;
    let mut pool = PhpForkPool::spawn(
        &fork_script, &autoload, bootstrap.as_deref(),
        &defines, &env_triples, &server_pairs, &ini_pairs,
        worker_count,
    )?;

    let stop_on = if cli.stop_on_defect {
        phpunit_rust::runner::StopOn::on_defect()
    } else if cli.stop_on_failure {
        phpunit_rust::runner::StopOn::on_failure()
    } else {
        phpunit_rust::runner::StopOn::default()
    };
    let cfg = RunConfig {
        autoload,
        bootstrap: None,
        filter: cli.filter,
        defines,
        stop_on,
    };
    let mut report = run(&mut pool, cases, &cfg, &row_counts, |o| print_progress(o))?;

    // Append the synthetic skip outcomes for @group legacy under Symfony's
    // listener. They were never dispatched, so emit them now and adjust
    // the report's totals accordingly.
    if !synthetic_legacy_skips.is_empty() {
        for o in &synthetic_legacy_skips { print_progress(o); }
        report.outcomes.extend(synthetic_legacy_skips);
    }

    print_summary(&report);

    #[cfg(feature = "coverage")]
    if let Some(fmt) = &cli.coverage_format {
        use phpunit_rust::coverage::{emit, passed_set};
        let allowed = passed_set(&report);
        let xml_path = xml_path.as_deref().ok_or_else(|| {
            anyhow::anyhow!("--coverage-format requires a phpunit.xml; use --configuration or place phpunit.xml in the project root")
        })?;
        emit(xml_path, Some(&allowed), fmt, cli.coverage_out.as_deref())
            .context("coverage analysis failed")?;
        if let Some(p) = &cli.coverage_out {
            eprintln!("Coverage written to {}", p.display());
        }
    }

    if report.is_success() { Ok(ExitCode::SUCCESS) } else { Ok(ExitCode::from(1)) }
}
