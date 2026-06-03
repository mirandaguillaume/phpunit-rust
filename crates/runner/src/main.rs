use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

use phpunit_rust::discovery::discover_with_index;

/// Parse composer.json's `autoload-dev` AND `autoload` PSR-4/classmap entries
/// into a list of directories, resolved relative to `project`. Used to build
/// a complete class graph for inheritance resolution (e.g. abstract base classes
/// in the main autoload like MockeryTestCase). Returns empty Vec if absent.
fn parse_autoload_dev_dirs(project: &std::path::Path) -> Vec<PathBuf> {
    let path = project.join("composer.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
        return vec![];
    };

    let mut dirs = Vec::new();
    for section in ["autoload-dev", "autoload"] {
        let Some(block) = val.get(section) else {
            continue;
        };
        collect_psr4_dirs(block, project, &mut dirs);
        collect_classmap_dirs(block, project, &mut dirs);
    }
    dirs
}

fn collect_psr4_dirs(block: &serde_json::Value, project: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Some(psr4) = block.get("psr-4").and_then(|v| v.as_object()) else {
        return;
    };
    for v in psr4.values() {
        match v {
            serde_json::Value::String(s) => {
                let p = project.join(s);
                if p.is_dir() {
                    out.push(p);
                }
            }
            serde_json::Value::Array(arr) => {
                for s in arr {
                    if let Some(s) = s.as_str() {
                        let p = project.join(s);
                        if p.is_dir() {
                            out.push(p);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_classmap_dirs(
    block: &serde_json::Value,
    project: &std::path::Path,
    out: &mut Vec<PathBuf>,
) {
    let Some(arr) = block.get("classmap").and_then(|v| v.as_array()) else {
        return;
    };
    for s in arr {
        if let Some(s) = s.as_str() {
            let p = project.join(s);
            if p.is_dir() {
                out.push(p);
            }
        }
    }
}

/// True if a test (its `class` FQCN, its `file`, and its `fingerprint` — the set
/// of FQCNs it references) is impacted by a change to any file in `changed_files`
/// or any class in `changed_fqcns`: its own file changed, its class is a changed
/// FQCN, or it references one. Pure; the heart of --dirty.
fn is_impacted(
    class: &str,
    file: &std::path::Path,
    fingerprint: &std::collections::HashSet<String>,
    changed_files: &std::collections::HashSet<PathBuf>,
    changed_fqcns: &std::collections::HashSet<String>,
) -> bool {
    changed_files.contains(file)
        || changed_fqcns.contains(class)
        || fingerprint.iter().any(|f| changed_fqcns.contains(f))
}

/// Files with uncommitted changes in `project`'s git repo (tracked diff vs HEAD
/// plus untracked, gitignored excluded), as absolute paths under the canonical
/// repo root. Empty if not a git repo / git fails — the caller decides what that
/// means.
fn git_changed_files(project: &std::path::Path) -> std::collections::HashSet<PathBuf> {
    use std::process::Command;
    let root = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| project.to_path_buf());
    let lines = |args: &[&str]| -> Vec<String> {
        Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut rel = lines(&["diff", "--name-only", "HEAD"]);
    rel.extend(lines(&["ls-files", "--others", "--exclude-standard"]));
    rel.into_iter().map(|r| root.join(r)).collect()
}

/// Outcome of the DB pre-flight decision.
#[derive(Debug, PartialEq)]
pub(crate) enum DbPreflight {
    /// Proceed normally (no DB tests, or DB is configured).
    Proceed,
    /// Skip DB tests and continue (--skip-db).
    SkipDbTests,
    /// Abort before forking: DB tests selected but nothing configured.
    Abort,
}

/// Pure decision helper: no I/O, no side effects.
/// `db_configured` = `--provision-db` set OR PHPUNIT_RUST_DB_DSN present.
pub(crate) fn db_preflight(selected_needs_db: bool, db_configured: bool, skip_db: bool) -> DbPreflight {
    match (selected_needs_db, db_configured, skip_db) {
        (false, _, _) => DbPreflight::Proceed,
        (true, true, _) => DbPreflight::Proceed,
        (true, false, true) => DbPreflight::SkipDbTests,
        (true, false, false) => DbPreflight::Abort,
    }
}

/// Returns `true` iff at least one of the FINAL selected test cases (after all
/// filters including `--filter`, `--group`, and `--dirty`) has `needs_db = true`.
/// When `false` the gate is a zero-cost no-op; no provisioning logic runs.
fn selected_needs_db(cases: &[phpunit_rust::types::TestCase]) -> bool {
    cases.iter().any(|c| c.needs_db)
}

#[cfg(test)]
mod dirty_tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    fn fps(xs: &[&str]) -> HashSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn impacted_when_file_class_or_fingerprint_changed() {
        let changed_files: HashSet<PathBuf> =
            ["/p/src/Calc.php"].iter().map(PathBuf::from).collect();
        let changed_fqcns: HashSet<String> = fps(&["App\\Calc"]);

        // (c) fingerprint references a changed class → impacted
        assert!(is_impacted(
            "Tests\\CalcTest",
            Path::new("/p/tests/CalcTest.php"),
            &fps(&["App\\Calc"]),
            &changed_files,
            &changed_fqcns
        ));
        // (a) the test's own file changed → impacted
        assert!(is_impacted(
            "Tests\\InSrc",
            Path::new("/p/src/Calc.php"),
            &fps(&[]),
            &changed_files,
            &changed_fqcns
        ));
        // unrelated test → NOT impacted
        assert!(!is_impacted(
            "Tests\\OtherTest",
            Path::new("/p/tests/OtherTest.php"),
            &fps(&["App\\Other"]),
            &changed_files,
            &changed_fqcns
        ));
    }
}

use phpunit_rust::fork_pool::PhpForkPool;
use phpunit_rust::php_worker::{check_php_version, find_enumerate_script, find_fork_script};
use phpunit_rust::phpunit_xml::{
    parse_bootstrap, parse_excluded_groups, parse_listeners, parse_php_block, parse_testsuites,
};
use phpunit_rust::provider_enum::{collect_provider_pairs, enumerate, RowCounts};
use phpunit_rust::reporter::{print_progress, print_summary};
use phpunit_rust::runner::RunConfig;
use phpunit_rust::types::{TestOutcome, TestStatus};

#[derive(Parser, Debug)]
#[command(
    name = "phpunit-rust",
    version,
    about = "PHPUnit-compatible test runner"
)]
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
    /// Inactivity watchdog: abort the run if no worker emits any output for
    /// this many seconds while tests are still in flight — catches a worker
    /// stuck in an infinite loop, a blocked syscall, or a hung sub-process
    /// (no SIGCHLD fires because the child is still alive). The stuck and
    /// not-yet-dispatched tests are reported as errors so the run finishes
    /// instead of hanging forever. Set to 0 to disable. Default: 600.
    #[arg(long, default_value_t = 600)]
    worker_timeout: u64,
    /// Print the full list of (class, method) tests that would be run
    /// for the current config (after group filtering and testsuite
    /// selection), then exit without running anything. Matches vanilla's
    /// --list-tests format.
    #[arg(long)]
    list_tests: bool,
    /// Run only tests impacted by uncommitted git changes: a changed source file
    /// maps (via the class graph) to the test classes whose fingerprint references
    /// it, plus changed test files themselves. Like Pest's --dirty but graph-based
    /// (changed source → dependent tests), not just changed test files. Not a git
    /// repo / no changes → nothing to run.
    #[arg(long)]
    dirty: bool,
    /// Base DSN for per-worker database provisioning (Phase 3). Passing this flag
    /// marks a database as "configured" for the preflight gate — actual
    /// provisioning is wired in Phase 3. Example: postgres://user:pw@localhost/mydb
    #[arg(long)]
    provision_db: Option<String>,
    /// When `needs_db` tests are selected but no database is configured,
    /// skip those tests instead of aborting (exit code 2). The skip reason
    /// "database not configured (--skip-db)" is recorded in the report.
    #[arg(long)]
    skip_db: bool,
    /// Rewrite `createMock()` patterns in test files into anonymous-class stubs
    /// before execution. Requires that mocked interfaces are resolvable via
    /// the project's PSR-4 autoload map in composer.json.
    #[arg(long)]
    bake_mocks: bool,
    /// PHP memory_limit applied inside each long-lived worker process.
    /// Accepts any value php.ini understands: "256M", "1G", "-1" (unlimited).
    /// Defaults to "512M" — generous enough for most suites without letting
    /// 8 workers collectively exhaust the host's RAM.
    #[arg(long, default_value = "512M")]
    worker_memory_limit: String,
    /// Recycle each PHP worker fork after it has processed this many batches.
    /// The master forks a fresh replacement that inherits its warm
    /// autoload/bootstrap state via COW — so per-fork accumulators (e.g.
    /// Symfony bridge deprecation collectors) can't blow the memory limit
    /// before recycling. Defaults to 20. Pass 0 to disable (long-lived
    /// workers, original behaviour). Counting happens in PHP master.
    #[arg(long, default_value = "20")]
    worker_max_batches: u32,
    /// Write a Chrome Trace Format JSON file timing every meaningful phase
    /// (discovery, autoload preload, fork pool spawn, per-batch dispatch and
    /// wait, aggregation, output). Load the file in `chrome://tracing`,
    /// Perfetto (perfetto.dev), or Speedscope (speedscope.app) to see where
    /// wall clock is being spent. Quasi-zero overhead when unset.
    #[arg(long)]
    profile: Option<PathBuf>,
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
    // Profiler clock starts at the earliest opportunity so wall-clock
    // accounting includes config parsing, not just test execution.
    let profiler = phpunit_rust::profiler::Profiler::new(cli.profile.is_some());
    let project = cli
        .project
        .canonicalize()
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
                if dist.is_file() {
                    Some(dist)
                } else {
                    None
                }
            }
        }
    };

    // Read the phpunit.xml once; reuse for bootstrap + testsuites + constants.
    let xml_str = match &xml_path {
        Some(xml) => Some(
            std::fs::read_to_string(xml).with_context(|| format!("reading {}", xml.display()))?,
        ),
        None => None,
    };

    let bootstrap = match (cli.bootstrap, xml_str.as_deref()) {
        (Some(b), _) => Some(if b.is_absolute() { b } else { project.join(b) }),
        (None, Some(xml)) => parse_bootstrap(xml).map(|rel| {
            let p = PathBuf::from(&rel);
            if p.is_absolute() {
                p
            } else {
                project.join(p)
            }
        }),
        (None, None) => None,
    };
    if let Some(b) = &bootstrap {
        eprintln!("Using bootstrap: {}", b.display());
    }

    // <php> block: const + env + server + ini, all in one walk over
    // the XML. Forwarded to the master so it can apply them once
    // before fork (so each child inherits the configured state via COW).
    let php_block = xml_str.as_deref().map(parse_php_block).unwrap_or_default();
    let defines: Vec<[String; 2]> = php_block
        .constants
        .iter()
        .map(|c| [c.name.clone(), c.value.clone()])
        .collect();
    let env_triples: Vec<(String, String, bool)> = php_block
        .env
        .iter()
        .map(|e| (e.name.clone(), e.value.clone(), e.force))
        .collect();
    let server_pairs: Vec<[String; 2]> = php_block
        .server
        .iter()
        .map(|s| [s.name.clone(), s.value.clone()])
        .collect();
    let ini_pairs: Vec<[String; 2]> = php_block
        .ini
        .iter()
        .map(|s| [s.name.clone(), s.value.clone()])
        .collect();
    // `<var>` populates $GLOBALS (PHPUnit PhpHandler semantics) — distinct from
    // `<env>`. Forwarded separately so the worker can set $GLOBALS, not putenv.
    let var_pairs: Vec<[String; 2]> = php_block
        .vars
        .iter()
        .map(|s| [s.name.clone(), s.value.clone()])
        .collect();
    let total =
        defines.len() + env_triples.len() + server_pairs.len() + ini_pairs.len() + var_pairs.len();
    if total > 0 {
        eprintln!(
            "Applying <php> block: {} const, {} env, {} server, {} ini, {} var",
            defines.len(),
            env_triples.len(),
            server_pairs.len(),
            ini_pairs.len(),
            var_pairs.len()
        );
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
                eprintln!(
                    "Selecting testsuite '{}' ({} suite{} matched)",
                    target,
                    suites.len(),
                    if suites.len() == 1 { "" } else { "s" }
                );
            }
            if suites.is_empty() {
                let dir = project.join(&cli.tests_dir);
                (vec![dir], vec![])
            } else {
                let mut roots = Vec::new();
                let mut excls = Vec::new();
                for s in suites {
                    for d in s.directories {
                        if !d.is_class_discoverable() {
                            // `.phpt` (and any other non-`.php` suffix) means
                            // PHPUnit only invokes specific file types in this
                            // dir — we don't support those formats, so skip
                            // it entirely instead of finding spurious classes
                            // (e.g. fixture files hidden in `_files/`).
                            continue;
                        }
                        let p = PathBuf::from(&d.path);
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
                eprintln!(
                    "warning: test directory not found, skipping: {}",
                    p.display()
                );
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
        eprintln!(
            "Discovering tests across {} roots ({} excludes)...",
            test_roots.len(),
            excludes.len()
        );
    }
    // Single-pass discovery + class-file index. Empirically faster than
    // splitting into cases-then-index on every benched project (the
    // double-parse cost on slow-path projects always exceeds the targeted
    // filter savings on fast-path ones).
    let (mut cases, class_file_index_full) = profiler.span_with(
        "discovery_and_index",
        "main",
        serde_json::json!({
            "roots": test_roots.len(),
            "excludes": excludes.len(),
            "supplement_dirs": graph_supplement_dirs.len(),
        }),
        || discover_with_index(&test_roots, &excludes, &graph_supplement_dirs),
    )?;

    // --dirty: keep only tests impacted by uncommitted git changes. Reverse the
    // full class→file index to map each changed file to the FQCNs it defines,
    // then retain a test whose own file changed, whose class is a changed FQCN,
    // or whose fingerprint references one. Graph-based impact (changed *source* →
    // dependent tests), unlike Pest's file-only --dirty. Runs before the index is
    // narrowed so the reduced case set drives the rest of the pipeline.
    if cli.dirty {
        let changed_files = git_changed_files(&project);
        if changed_files.is_empty() {
            eprintln!("--dirty: no uncommitted changes (or not a git repo); nothing to run.");
            cases.clear();
        } else {
            let mut changed_fqcns: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for (fqcn, file) in &class_file_index_full {
                if changed_files.contains(file) {
                    changed_fqcns.insert(fqcn.clone());
                }
            }
            let before = cases.len();
            cases.retain(|c| {
                is_impacted(
                    &c.class,
                    &c.file,
                    &c.fingerprint,
                    &changed_files,
                    &changed_fqcns,
                )
            });
            eprintln!(
                "--dirty: {} of {} test methods impacted by {} changed file(s).",
                cases.len(),
                before,
                changed_files.len()
            );
        }
    }

    // Build a FQCN→file index over all PHP files in test roots and supplement
    // dirs so the runner can locate classes the test code references but the
    // PSR-4 autoloader can't reach. We then filter the index down to *only*
    // the FQCNs the tests can statically reach: their #[DataProviderExternal]
    // pairs (those are explicit by name) AND every class FQCN that appears
    // in any test method's body fingerprint — `new Foo()`, `Foo::class`,
    // `createMock(Foo::class)`, `Foo::CONST`, `instanceof Foo`, …
    //
    // This mirrors PHPUnit's behaviour (it never indexes a class until
    // reflection demands it). A full scan would index every fixture file
    // rector ships (~1500 entries), bloat the PHP master's classMapExtra
    // map, and make the opcache pre-warm loop fatal on suites with
    // thousands of fixture stubs.
    // PSR-4 sufficiency check: if composer's autoload covers EVERY class
    // the discovered tests reference (their own FQCN + fingerprint hits +
    // external provider FQCNs), the runner doesn't need to ship its own
    // file-path fallback to the worker — composer.php's PSR-4 autoload
    // will find each class. We skip the file-tree walk entirely in that
    // case (the dominant runtime cost on large projects).
    let mut wanted_fqcns: std::collections::HashSet<String> = cases
        .iter()
        .flat_map(|c| {
            c.external_providers
                .iter()
                .map(|(fqcn, _)| fqcn.clone())
                .chain(c.fingerprint.iter().cloned())
        })
        .collect();
    // The test classes themselves must also be resolvable through the
    // fallback map so the worker can `require_once` them before running
    // their methods — they aren't necessarily in their own fingerprint
    // (a method body rarely references its enclosing class).
    for c in &cases {
        wanted_fqcns.insert(c.class.clone());
    }
    let class_file_index: std::collections::HashMap<String, PathBuf> = class_file_index_full
        .into_iter()
        .filter(|(fqcn, _)| wanted_fqcns.contains(fqcn))
        .collect();

    // Honor phpunit.xml's <groups><exclude>: drop any test whose effective
    // groups include one of the excluded names. Vanilla PHPUnit does this
    // at run time; we do it at discovery so the dispatch queue and the
    // outcome count match vanilla's.
    //
    // CLI flags --group / --exclude-group layer ON TOP:
    //   * --exclude-group adds names to the XML's exclude list.
    //   * --group restricts to tests whose groups intersect the include set
    //     (PHPUnit semantics: only the named groups run).
    let mut excluded_groups: Vec<String> = xml_str
        .as_deref()
        .map(parse_excluded_groups)
        .unwrap_or_default();
    for g in &cli.exclude_groups {
        if !excluded_groups.contains(g) {
            excluded_groups.push(g.clone());
        }
    }
    if !excluded_groups.is_empty() {
        use std::collections::HashSet;
        let excl: HashSet<&str> = excluded_groups.iter().map(|s| s.as_str()).collect();
        let before = cases.len();
        cases.retain(|c| !c.groups.iter().any(|g| excl.contains(g.as_str())));
        let dropped = before - cases.len();
        eprintln!(
            "Excluding {} test{} in groups: {}",
            dropped,
            if dropped == 1 { "" } else { "s" },
            excluded_groups.join(", ")
        );
    }
    if !cli.groups.is_empty() {
        use std::collections::HashSet;
        let incl: HashSet<&str> = cli.groups.iter().map(|s| s.as_str()).collect();
        let before = cases.len();
        cases.retain(|c| c.groups.iter().any(|g| incl.contains(g.as_str())));
        eprintln!(
            "Including only {} test{} in group{} {} (filtered {} → {})",
            cases.len(),
            if cases.len() == 1 { "" } else { "s" },
            if cli.groups.len() == 1 { "" } else { "s" },
            cli.groups.join(", "),
            before,
            cases.len()
        );
    }

    // --filter is applied HERE — the single place the substring predicate
    // runs against the FINAL selected set (after --dirty, --exclude-group and
    // --group). The post-spawn application in run_with_profiler then sees an
    // already-narrowed set (RunConfig.filter is None below), so there is
    // exactly one filter application and which tests run is unchanged.
    if let Some(f) = cli.filter.as_deref() {
        let before = cases.len();
        cases.retain(|c| phpunit_rust::runner::matches_filter(&c.class, &c.method, Some(f)));
        eprintln!(
            "--filter {:?}: {} of {} test method(s) match.",
            f,
            cases.len(),
            before
        );
    }

    // Symfony's PhpUnitTestsListener detection is intentionally NOT acted
    // upon: the listener's "SkippedTestCase wrapper" behaviour isn't
    // "every @group legacy test" — it inspects deprecation emissions at
    // run-time and conditionally skips. Replicating that without running
    // the listener itself is unsound (initial attempt over-skipped 526
    // cases when vanilla wraps only 14). Leaving this stub so we know
    // the detection is wired up if we later add generic <listeners>
    // dispatch.
    let _listeners: Vec<String> = xml_str.as_deref().map(parse_listeners).unwrap_or_default();
    let synthetic_legacy_skips: Vec<phpunit_rust::types::TestOutcome> = Vec::new();

    eprintln!(
        "Found {} test methods across {} classes.",
        cases.len(),
        cases
            .iter()
            .map(|c| &c.class)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );

    // --bake-mocks: rewrite createMock patterns into anonymous classes before
    // dispatching. The temp_dir must outlive the pool so baked PHP files are
    // still on disk when the worker tries to require them.
    let _bake_temp_dir: Option<tempfile::TempDir>;
    if cli.bake_mocks {
        let td = tempfile::TempDir::new().context("creating temp dir for baked test files")?;
        let rewritten = phpunit_rust::mock_bake::bake_test_cases(&cases, &project, &td);
        let baked_count = rewritten
            .iter()
            .zip(cases.iter())
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

    // Demand gate: only perform DB-related work when the FINAL selected set
    // actually contains needs_db cases. When false this is a zero-cost no-op
    // and the hot path is byte-identical to a pre-P1 run.
    let needs_db = selected_needs_db(&cases);

    // DB preflight: fail-fast (or skip) when needs_db tests are selected but
    // no database is configured. "configured" = --provision-db set OR
    // PHPUNIT_RUST_DB_DSN present. The gate is zero-cost when needs_db = false.
    let db_configured = cli.provision_db.is_some()
        || std::env::var_os("PHPUNIT_RUST_DB_DSN").is_some();
    let db_case_count = cases.iter().filter(|c| c.needs_db).count();
    let mut synthetic_db_skips: Vec<TestOutcome> = Vec::new();
    match db_preflight(needs_db, db_configured, cli.skip_db) {
        DbPreflight::Proceed => {}
        DbPreflight::SkipDbTests => {
            eprintln!("note: skipping {db_case_count} test(s) that require a database (--skip-db)");
            for c in cases.iter().filter(|c| c.needs_db) {
                let o = TestOutcome {
                    class: c.class.clone(),
                    method: c.method.clone(),
                    dataset: None,
                    status: TestStatus::Skipped,
                    message: Some("database not configured (--skip-db)".into()),
                    trace: None,
                    duration_ms: 0.0,
                };
                print_progress(&o);
                synthetic_db_skips.push(o);
            }
            cases.retain(|c| !c.needs_db);
        }
        DbPreflight::Abort => {
            eprintln!(
                "error: {db_case_count} selected test(s) require a database but none is configured.\n  \
                 pass --provision-db <DSN_BASE> to provision per-worker databases, or --skip-db to skip them."
            );
            std::process::exit(2);
        }
    }

    // Verify a usable PHP is on PATH. We require ≥ 8.1; some projects need
    // newer (brick/math: 8.2; doctrine/collections: 8.4). The user is on
    // the hook for installing a sufficiently-new PHP.
    let php_id =
        check_php_version(80100).context("PHP version check failed (need ≥ 8.1 on PATH)")?;
    eprintln!("PHP version id: {php_id}");

    // Enumerate data-provider row counts BEFORE forking workers.
    // The runner uses these to decide whether to split a heavy provider
    // method into multiple stride-partitioned plans (see build_queue).
    // A failed enumeration is non-fatal: missing entries fall back to
    // single-bucket dispatch.
    let provider_pairs = collect_provider_pairs(&cases);
    let row_counts: RowCounts = profiler.span_with(
        "enumerate_providers",
        "main",
        serde_json::json!({"pairs": provider_pairs.len()}),
        || -> Result<_> {
            if provider_pairs.is_empty() {
                return Ok(RowCounts::new());
            }
            let enum_script = find_enumerate_script()?;
            Ok(
                match enumerate(
                    &enum_script,
                    &autoload,
                    bootstrap.as_deref(),
                    &defines,
                    &provider_pairs,
                ) {
                    Ok(counts) => counts,
                    Err(e) => {
                        eprintln!(
                            "Provider enumeration failed (continuing with no row data): {e:#}"
                        );
                        RowCounts::new()
                    }
                },
            )
        },
    )?;

    eprintln!(
        "Spawning {} PHP worker{}...",
        worker_count,
        if worker_count == 1 { "" } else { "s" }
    );
    let fork_script = find_fork_script()?;
    let mut pool = profiler.span_with(
        "fork_pool_spawn",
        "main",
        serde_json::json!({"workers": worker_count}),
        || -> Result<_> {
            PhpForkPool::spawn(
                &fork_script,
                &autoload,
                bootstrap.as_deref(),
                &defines,
                &env_triples,
                &server_pairs,
                &ini_pairs,
                &var_pairs,
                worker_count,
                &class_file_index,
                &cli.worker_memory_limit,
                cli.worker_max_batches,
            )
        },
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
        // filter already applied in main.rs via matches_filter; runner sees None.
        filter: None,
        defines,
        stop_on,
        class_file_index,
        n_workers: worker_count,
        worker_timeout: (cli.worker_timeout > 0)
            .then(|| std::time::Duration::from_secs(cli.worker_timeout)),
    };
    let n_cases = cases.len();
    let mut report = profiler.span_with(
        "run",
        "main",
        serde_json::json!({"cases": n_cases, "workers": worker_count}),
        || {
            phpunit_rust::runner::run_with_profiler(
                &mut pool,
                cases,
                &cfg,
                &row_counts,
                print_progress,
                &profiler,
            )
        },
    )?;

    // Append synthetic skips for DB tests suppressed by --skip-db.
    if !synthetic_db_skips.is_empty() {
        report.outcomes.extend(synthetic_db_skips);
    }

    // Append the synthetic skip outcomes for @group legacy under Symfony's
    // listener. They were never dispatched, so emit them now and adjust
    // the report's totals accordingly.
    if !synthetic_legacy_skips.is_empty() {
        for o in &synthetic_legacy_skips {
            print_progress(o);
        }
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

    // Write the profile JSON if requested. Done late so the trace covers
    // every phase up to (but not including) this write — the write itself
    // is fast (microseconds for a typical 50-event trace).
    if let Some(out) = &cli.profile {
        if let Err(e) = profiler.write_to(out) {
            eprintln!("warning: writing profile to {} failed: {e}", out.display());
        } else {
            eprintln!(
                "Profile written to {} ({} events). \
                Open it in chrome://tracing, perfetto.dev, or speedscope.app.",
                out.display(),
                profiler.event_count()
            );
        }
    }

    if report.is_success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use phpunit_rust::types::TestCase;
    use std::path::PathBuf;

    fn case(class: &str, method: &str, needs_db: bool) -> TestCase {
        TestCase {
            file: PathBuf::from("/p/T.php"),
            class: class.to_string(),
            method: method.to_string(),
            data_provider: None,
            groups: vec![],
            external_providers: vec![],
            is_tautological: false,
            has_lifecycle_overrides: false,
            depends_on: vec![],
            is_dispatch_safe: true,
            fingerprint: std::collections::HashSet::new(),
            is_stateful: false,
            is_isolated: false,
            needs_db,
        }
    }

    #[test]
    fn gate_is_noop_when_nothing_needs_db() {
        let cases = vec![case("A", "t1", false), case("B", "t2", false)];
        assert!(!selected_needs_db(&cases), "no-DB suite must NOT trip the gate");
    }

    #[test]
    fn gate_trips_when_any_selected_case_needs_db() {
        let cases = vec![case("A", "t1", false), case("B", "t2", true)];
        assert!(selected_needs_db(&cases));
    }

    #[test]
    fn db_preflight_decisions() {
        use super::{db_preflight, DbPreflight};
        // No DB tests selected -> always proceed (zero-cost path).
        assert_eq!(db_preflight(false, false, false), DbPreflight::Proceed);
        assert_eq!(db_preflight(false, false, true), DbPreflight::Proceed);
        // DB tests + a configured database -> proceed.
        assert_eq!(db_preflight(true, true, false), DbPreflight::Proceed);
        // DB tests, no database, --skip-db -> skip them.
        assert_eq!(db_preflight(true, false, true), DbPreflight::SkipDbTests);
        // DB tests, no database, no --skip-db -> abort.
        assert_eq!(db_preflight(true, false, false), DbPreflight::Abort);
    }

    #[test]
    fn filter_lift_does_not_change_selected_set_or_gate() {
        // Simulate: two cases, one matches the filter, one doesn't.
        // After retain, only the matching case remains. Gate checks THAT set.
        let filter = "DbTest";
        let mut cases = vec![
            case("DbTest", "testSave", true),
            case("OtherTest", "testFoo", false),
        ];
        cases.retain(|c| phpunit_rust::runner::matches_filter(&c.class, &c.method, Some(filter)));
        assert_eq!(cases.len(), 1);
        assert!(selected_needs_db(&cases), "filtered set still needs DB");
    }
}
