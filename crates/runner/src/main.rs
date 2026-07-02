use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use proust::discovery::{
    discover_cases_and_test_index, discover_nontest_class_index, discover_with_index,
};

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

/// First single-quoted substring in `s`, or None.
fn first_single_quoted(s: &str) -> Option<String> {
    let start = s.find('\'')? + 1;
    let rest = &s[start..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// Parse `vendor/composer/autoload_psr4.php` into (namespace prefix, absolute dir) pairs.
/// Returns None when the file is absent or yields nothing — the caller then takes the full
/// `discover_with_index` path (never worse than today). PSR-4 only; classmap/psr-0 are
/// intentionally ignored (an unmatched FQCN is treated as unresolvable and kept via the
/// full-parse fallback, so this can never wrongly drop a class).
fn load_composer_psr4_map(project: &Path) -> Option<Vec<(String, PathBuf)>> {
    let text = std::fs::read_to_string(project.join("vendor/composer/autoload_psr4.php")).ok()?;
    let vendor_dir = project.join("vendor");
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for line in text.lines() {
        let Some(arrow) = line.find("=>") else {
            continue;
        };
        let (head, tail) = line.split_at(arrow);
        let Some(prefix_raw) = first_single_quoted(head) else {
            continue;
        };
        if !prefix_raw.ends_with('\\') {
            continue;
        }
        let prefix = prefix_raw.replace("\\\\", "\\");
        let mut search = tail;
        loop {
            let v = search.find("$vendorDir");
            let b = search.find("$baseDir");
            let (is_vendor, at) = match (v, b) {
                (Some(vi), Some(bi)) => {
                    if vi < bi {
                        (true, vi)
                    } else {
                        (false, bi)
                    }
                }
                (Some(vi), None) => (true, vi),
                (None, Some(bi)) => (false, bi),
                (None, None) => break,
            };
            let after = &search[at..];
            let Some(suffix) = first_single_quoted(after) else {
                break;
            };
            let rel = suffix.trim_start_matches('/');
            let dir = if is_vendor {
                vendor_dir.join(rel)
            } else {
                project.join(rel)
            };
            out.push((prefix.clone(), dir));
            let q = after.find('\'').unwrap_or(0) + 1;
            let qend = after[q..]
                .find('\'')
                .map(|e| q + e + 1)
                .unwrap_or(after.len());
            search = &after[qend..];
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// True when composer's PSR-4 autoloader would resolve `fqcn` to an existing file —
/// i.e. some prefix matches and the derived path is on disk. When true, the runner's
/// own class-map fallback entry for `fqcn` is dead weight (the worker autoloads it via
/// composer), so it can be omitted from the shipped index.
fn is_psr4_resolvable(fqcn: &str, psr4: &[(String, PathBuf)]) -> bool {
    let fqcn = fqcn.trim_start_matches('\\');
    for (prefix, dir) in psr4 {
        let pfx = prefix.trim_start_matches('\\');
        if let Some(rest) = fqcn.strip_prefix(pfx) {
            let rel = rest.replace('\\', "/");
            if dir.join(format!("{rel}.php")).is_file() {
                return true;
            }
        }
    }
    false
}

/// Discover test cases and the FQCN→file fallback index. When `need_full_index` is false
/// and composer's PSR-4 map resolves every class the tests reference, parse only `*Test*.php`
/// (the fast path — skips the production `src/` tree) and ship a test-only index. Otherwise
/// fall back to the full single-pass `discover_with_index` (identical to prior behavior).
///
/// Parity: the emission graph (and thus the test count) is built only from `*Test*.php` on
/// every path, so it is invariant. The index is only ever a PSR-4-MISS fallback in the worker
/// (worker_fork.php), so omitting entries for PSR-4-resolvable classes changes nothing at run
/// time; when ANY referenced class is not PSR-4-resolvable we take the full parse, so the index
/// is never smaller than what the worker can actually use.
fn build_cases_and_index(
    project: &Path,
    roots: &[PathBuf],
    excludes: &[PathBuf],
    supplement_dirs: &[PathBuf],
    need_full_index: bool,
) -> anyhow::Result<(
    Vec<proust::types::TestCase>,
    std::collections::HashMap<String, PathBuf>,
)> {
    if !need_full_index {
        if let Some(psr4) = load_composer_psr4_map(project) {
            let (cases, test_class_index) =
                discover_cases_and_test_index(roots, excludes, supplement_dirs)?;

            let mut wanted: std::collections::HashSet<String> = cases
                .iter()
                .flat_map(|c| {
                    c.external_providers
                        .iter()
                        .map(|(fqcn, _)| fqcn.clone())
                        .chain(c.fingerprint.iter().cloned())
                })
                .collect();
            for c in &cases {
                wanted.insert(c.class.clone());
            }

            let all_resolvable = wanted
                .iter()
                .all(|f| test_class_index.contains_key(f) || is_psr4_resolvable(f, &psr4));
            if all_resolvable {
                return Ok((cases, test_class_index));
            }
            // Fallback WITHOUT re-parsing the test files: parse only the non-test files and
            // merge with the already-parsed test-file index. Byte-identical to
            // discover_with_index's (cases, index) for any suite without a cross-file FQCN
            // redeclaration (which is a PHP fatal). Avoids the double test-file parse.
            let dirs: Vec<PathBuf> = roots
                .iter()
                .chain(supplement_dirs.iter())
                .cloned()
                .collect();
            let mut index = discover_nontest_class_index(&dirs, excludes);
            for (fqcn, file) in test_class_index {
                index.entry(fqcn).or_insert(file);
            }
            return Ok((cases, index));
        }
    }
    let (cases, index) = discover_with_index(roots, excludes, supplement_dirs)?;
    Ok((cases, index))
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
/// `db_configured` = `--provision-db` set OR PROUST_DB_DSN present.
pub(crate) fn db_preflight(
    selected_needs_db: bool,
    db_configured: bool,
    skip_db: bool,
) -> DbPreflight {
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
fn selected_needs_db(cases: &[proust::types::TestCase]) -> bool {
    cases.iter().any(|c| c.needs_db)
}

/// True when ANY selected test extends a known functional framework base class
/// (Symfony `KernelTestCase`/`WebTestCase`, …) — a high per-worker fixed cost
/// (cold kernel boot) even without `--provision-db`. Drives the conservative
/// worker clamp and the `--warmup` suggestion. Marker-based (declared `extends`),
/// never type-reference inference; affects only worker count + a hint, never
/// correctness.
fn selected_is_functional(cases: &[proust::types::TestCase]) -> bool {
    cases.iter().any(|c| c.is_functional)
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

use proust::fork_pool::PhpForkPool;
use proust::php_worker::{check_php_version, find_fork_script};
use proust::phpunit_xml::{
    parse_bootstrap, parse_excluded_groups, parse_listeners, parse_php_block, parse_testsuites,
};
use proust::provider_enum::RowCounts;
use proust::reporter::{print_progress, print_summary};
use proust::runner::RunConfig;
use proust::types::{TestOutcome, TestStatus};

#[derive(Parser, Debug)]
#[command(name = "proust", version, about = "PHPUnit-compatible test runner")]
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
    /// Optional warmup PHP file run ONCE in the fork master before any worker is
    /// forked (after --bootstrap, just before the fork). Forked workers inherit
    /// its warm state via copy-on-write — e.g. a file that boots your framework
    /// kernel so each worker skips the cold ~90ms first-boot. Best-effort: a
    /// warmup error warns and the run continues unwarmed. Unlike --bootstrap it
    /// is NOT loaded by the provisioning/teardown helpers, so its cost is paid
    /// exactly once. Overrides the PROUST_WARMUP environment variable.
    #[arg(long)]
    warmup: Option<PathBuf>,
    /// Path to phpunit.xml. Defaults to <project>/phpunit.xml, then
    /// phpunit.dist.xml, then phpunit.xml.dist if found. We extract: the `bootstrap` attribute,
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
    /// Print a SharedTransactionalFixture eligibility advisory for the selected test
    /// roots (one tab-separated line per concrete test class: uses-trait, eligible,
    /// reason; plus a `WARN` line per ineligible trait user and an `eligible: N/total`
    /// summary), then exit without running anything. Read-only; no runtime effect.
    #[arg(long)]
    report_shared_fixture: bool,
    /// Print a Way-3 setUp-hoist advisory: per concrete test class, which setUp
    /// `$this->P = …` fixtures could be hoisted to run ONCE (HOIST) vs why not
    /// (REFUSE: non-determinism / per-test ambient context / mutation), with the
    /// per-class test multiplicity and a `hoistable: H/total` summary. Read-only,
    /// tree-sitter-only; then exit without running anything.
    #[arg(long)]
    report_hoistable_setup: bool,
    /// Run only tests impacted by uncommitted git changes: a changed source file
    /// maps (via the class graph) to the test classes whose fingerprint references
    /// it, plus changed test files themselves. Like Pest's --dirty but graph-based
    /// (changed source → dependent tests), not just changed test files. Not a git
    /// repo / no changes → nothing to run.
    #[arg(long)]
    dirty: bool,
    /// Base DSN for per-worker database provisioning. Marks a database as
    /// "configured" for the preflight gate and creates one clone per worker
    /// (CREATE DATABASE … TEMPLATE). Example: postgres://user:pw@localhost/mydb
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
    /// Write a PHPUnit-compatible JUnit XML report to this file. Consumed as-is by
    /// GitLab, GitHub Actions, and the Jenkins JUnit plugin. Same flag name as PHPUnit.
    #[arg(long = "log-junit")]
    log_junit: Option<PathBuf>,
    /// Print a TestDox view (human-readable "it does X" sentences, grouped by class)
    /// after the run.
    #[arg(long)]
    testdox: bool,
    /// Write the TestDox report to this file as plain text. Same flag name as PHPUnit.
    #[arg(long = "log-testdox-text")]
    log_testdox_text: Option<PathBuf>,
    /// Write a Clover XML coverage report to this file. Runtime coverage — needs the
    /// pcov or xdebug extension. Same flag name as PHPUnit. (Distinct from the static
    /// `--coverage-format`, which needs no extension; see COMPATIBILITY.)
    #[arg(long = "coverage-clover")]
    coverage_clover: Option<PathBuf>,
    /// Write an HTML coverage report to this directory. Runtime — needs pcov/xdebug.
    #[arg(long = "coverage-html")]
    coverage_html: Option<PathBuf>,
    /// Text coverage summary. Runtime — needs pcov/xdebug. Bare `--coverage-text`
    /// prints to stdout; pass a path to write a file. Same flag name as PHPUnit.
    #[arg(long = "coverage-text", num_args = 0..=1, default_missing_value = "-")]
    coverage_text: Option<PathBuf>,
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

/// PHP fork-pool worker count, clamped by suite size. When the operator did NOT pass `--workers`
/// (explicit=false), never fork more children than `ceil(cases_len / k)` — a 12-test suite forks 1
/// child, not num_cpus, killing the small-suite fork-storm. An explicit `--workers N` is honored
/// verbatim. Floor 1. Pure for unit testing; the discovery rayon pool keeps the full count.
fn clamp_php_workers(requested: usize, explicit: bool, cases_len: usize, k: usize) -> usize {
    if explicit {
        return requested.max(1);
    }
    let by_size = cases_len.div_ceil(k.max(1)).max(1);
    requested.min(by_size).max(1)
}

/// Cases-per-worker divisor for the default (non-explicit) worker clamp.
/// A *provisioned* suite (`--provision-db`) is functional: each worker pays a
/// much larger fixed cost — a per-worker DB clone plus a cold framework-kernel
/// boot (~90ms unless `--warmup`) — so a worker only pays off over more tests.
/// We require twice as many cases before forking another worker, which keeps a
/// small functional suite at a worker count near vanilla instead of over-forking
/// (a 53-test Symfony suite goes 4→2 workers, lifting a measured +5% regression
/// to parity). Cheap unit suites keep the lower divisor and full parallelism.
const CLAMP_K_DEFAULT: usize = 16;
const CLAMP_K_PROVISIONED: usize = 32;
fn worker_clamp_k(provisioned: bool) -> usize {
    if provisioned {
        CLAMP_K_PROVISIONED
    } else {
        CLAMP_K_DEFAULT
    }
}

/// Max cases for the single-process (no-fork) fast path. Aligned with `clamp_php_workers`'s
/// `k=16`: in default mode the worker count only collapses to 1 for suites this small, so the
/// inline path naturally served exactly the tiny suites it was designed for. The cap makes that
/// bound explicit so an EXPLICIT `--workers 1` on a large suite does NOT inline.
const INLINE_MAX_CASES: usize = 16;

/// Locate a PHPUnit config under `project` by PHPUnit's filename precedence: a
/// committed `phpunit.xml` wins over a distributed template, and BOTH dist
/// spellings are recognized — `phpunit.dist.xml` (modern PHPUnit / Symfony Flex)
/// and the legacy `phpunit.xml.dist`. Returns the first that exists.
///
/// Recognizing `phpunit.dist.xml` is load-bearing for framework apps: Symfony's
/// standard layout ships that name, and missing it means the whole `<php>` block
/// is skipped — so `APP_ENV=test` never reaches the bootstrap and a Dotenv-driven
/// `KERNEL_CLASS`/`DATABASE_URL` is never loaded, failing every functional test.
fn autodetect_config_path(project: &std::path::Path) -> Option<PathBuf> {
    ["phpunit.xml", "phpunit.dist.xml", "phpunit.xml.dist"]
        .iter()
        .map(|name| project.join(name))
        .find(|p| p.is_file())
}

/// True when the suite should take the single-process (no-fork) fast path: exactly one PHP worker,
/// a tiny suite (≤ [`INLINE_MAX_CASES`]), AND every class is pure-CPU dispatch-safe — no
/// global-state mutation (is_stateful), no `@runInSeparateProcess` (is_isolated), no DB (needs_db).
///
/// The size cap is load-bearing: the inline path runs every test in ONE process with no fork
/// isolation or per-K recycling, so a single test that fatals/`exit()`s/segfaults takes down the
/// whole run (the runner then synthesises errors for every remaining test). That is acceptable for
/// a tiny suite — vanilla PHPUnit is single-process too — but catastrophic at scale (e.g. an
/// explicit `--workers 1` on doctrine-orm's 3004 tests lost all of them to one crash). Large suites
/// must use the fork pool, whose children are isolated and recycled, so a crash is contained to one
/// batch and the worker respawns. Matches vanilla's single-process model only for the tiny suites
/// where vanilla wins today.
fn single_process_eligible(workers: usize, cases: &[proust::types::TestCase]) -> bool {
    workers == 1
        && cases.len() <= INLINE_MAX_CASES
        && cases
            .iter()
            .all(|c| !c.is_stateful && !c.is_isolated && !c.needs_db)
}

fn real_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    // Profiler clock starts at the earliest opportunity so wall-clock
    // accounting includes config parsing, not just test execution.
    let profiler = proust::profiler::Profiler::new(cli.profile.is_some());
    let project = cli
        .project
        .canonicalize()
        .with_context(|| format!("project path invalid: {}", cli.project.display()))?;
    let autoload = project.join("vendor/autoload.php");
    // The read-only --report-shared-fixture advisory is tree-sitter-only (no PHP
    // execution), so it must not require `composer install`.
    if !autoload.is_file() && !cli.report_shared_fixture && !cli.report_hoistable_setup {
        return Err(anyhow!(
            "autoload not found at {}; run `composer install` first",
            autoload.display()
        ));
    }

    let xml_path = match cli.configuration {
        Some(p) => Some(if p.is_absolute() { p } else { project.join(p) }),
        None => autodetect_config_path(&project),
    };

    // Read the phpunit.xml once; reuse for bootstrap + testsuites + constants.
    let xml_str = match &xml_path {
        Some(xml) => Some(
            std::fs::read_to_string(xml).with_context(|| format!("reading {}", xml.display()))?,
        ),
        None => None,
    };

    // Delegated runtime coverage: when any --coverage-{clover,html,text} is set, hand
    // every worker a directory (via the inherited PROUST_COVERAGE_DIR env) to drop its
    // per-worker .cov file in; the merge + reports run after the test loop. Needs a
    // coverage driver (pcov/xdebug). `cov_dir_guard` keeps the tempdir alive until then.
    // Set BEFORE any worker is spawned so the master inherits it (still single-threaded
    // here — discovery/rayon hasn't started).
    let runtime_coverage =
        cli.coverage_clover.is_some() || cli.coverage_html.is_some() || cli.coverage_text.is_some();
    let cov_dir_guard = if runtime_coverage {
        if !proust::coverage_runtime::driver_available("php") {
            return Err(anyhow!(
                "--coverage-clover/--coverage-html/--coverage-text need a runtime coverage \
                 driver — install the pcov (or xdebug) PHP extension. For extension-free static \
                 coverage, build with --features coverage and use --coverage-format."
            ));
        }
        let dir = tempfile::TempDir::new().context("creating coverage temp dir")?;
        std::env::set_var("PROUST_COVERAGE_DIR", dir.path());
        // The worker scopes its Filter to <source> from this exact config path
        // (honours --configuration; the worker's own $__cfgPath is unreliable).
        if let Some(xml) = &xml_path {
            std::env::set_var("PROUST_CONFIG_PATH", xml);
        }
        // Clone the autoload path now — it is moved into the batch plan later, but
        // the post-run merge still needs it to load php-code-coverage.
        Some((dir, autoload.clone()))
    } else {
        None
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
        || {
            build_cases_and_index(
                &project,
                &test_roots,
                &excludes,
                &graph_supplement_dirs,
                cli.dirty,
            )
        },
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
        cases.retain(|c| proust::runner::matches_filter(&c.class, &c.method, Some(f)));
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
    let synthetic_legacy_skips: Vec<proust::types::TestOutcome> = Vec::new();

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
        let rewritten = proust::mock_bake::bake_test_cases(&cases, &project, &td);
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

    // --report-shared-fixture: advisory only. Verdict each concrete test class in the
    // selected roots and print the report, then exit (no tests run, no DB touched).
    if cli.report_shared_fixture {
        let mut report = Vec::new();
        for root in &test_roots {
            report.extend(proust::discovery::shared_fixture_report_in_dir(root)?);
        }
        report.sort_by(|a, b| a.fqcn.cmp(&b.fqcn));
        print!(
            "{}",
            proust::discovery::format_shared_fixture_report(&report)
        );
        return Ok(ExitCode::SUCCESS);
    }

    // --report-hoistable-setup: Way-3 setUp-hoist advisory. Verdict each concrete
    // class's setUp candidates and print, then exit (no tests run, no DB touched).
    if cli.report_hoistable_setup {
        let mut report = Vec::new();
        for root in &test_roots {
            report.extend(proust::discovery::setup_hoist_report_in_dir(root)?);
        }
        report.sort_by(|a, b| a.fqcn.cmp(&b.fqcn));
        print!("{}", proust::discovery::format_setup_hoist_report(&report));
        return Ok(ExitCode::SUCCESS);
    }

    // Demand gate: only perform DB-related work when the FINAL selected set
    // actually contains needs_db cases. When false this is a zero-cost no-op
    // and the hot path is byte-identical to a pre-P1 run.
    let needs_db = selected_needs_db(&cases);

    // DB preflight: fail-fast (or skip) when needs_db tests are selected but
    // no database is configured. "configured" = --provision-db set OR
    // PROUST_DB_DSN present. The gate is zero-cost when needs_db = false.
    let db_configured = cli.provision_db.is_some() || std::env::var_os("PROUST_DB_DSN").is_some();
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

    // L1: clamp the PHP fork-pool worker count by suite size (default only). The discovery rayon
    // pool above already used the unclamped count; from here, `worker_count` is the clamped value,
    // so a tiny suite spawns 1 worker instead of num_cpus (kills the small-suite fork-storm).
    // A functional suite (framework kernel boot) carries the same high
    // per-worker fixed cost as a provisioned one even without --provision-db, so
    // it gets the same conservative clamp. Marker-based detection (declared
    // `extends KernelTestCase/WebTestCase/…`); reused below for the warmup hint.
    let is_functional_suite = selected_is_functional(&cases);
    let worker_count = clamp_php_workers(
        worker_count,
        cli.workers.is_some(),
        cases.len(),
        worker_clamp_k(cli.provision_db.is_some() || is_functional_suite),
    );
    // L3: single-process (no-fork) fast path when we'd use exactly one worker AND every class is
    // pure-CPU dispatch-safe. The master runs the per-batch loop itself, matching vanilla's
    // single-process timing for the tiny suites where vanilla wins.
    let single_process = single_process_eligible(worker_count, &cases);

    // Data-provider row enumeration was removed. In production it produced no
    // usable data anyway — the standalone enumerator could not load
    // \Proust\TestExecutor, so its skip-reason guard always threw and every
    // provider degraded to null (= single-bucket dispatch). And once made to
    // work, stride-splitting measured net-SLOWER on every OSS suite (the
    // per-chunk dispatch overhead outweighs the parallelism gain for fast
    // provider rows: faker +18%, doctrine +8%). Dropping the enumerator removes
    // a whole PHP process (boot + autoload + ~45ms) from the startup floor.
    // `row_counts` stays an empty map: build_queue then dispatches every method
    // as a single bucket, exactly matching the prior production behavior.
    let row_counts = RowCounts::new();

    // Task 8: demand-gated lease build. Declared here so `_lease_guard` is in
    // scope BEFORE `pool` — Rust drops in reverse declaration order, so the
    // guard (and its destroy_all) runs AFTER the pool is dropped.
    let mut per_slot_dsn_opt: Option<Vec<String>> = None;
    let _lease_guard = if let Some(base) = &cli.provision_db {
        // Explicit --provision-db is a request to give each worker its OWN
        // database, so we always provision here — never gate on the marker-based
        // `needs_db` detection. Framework apps that isolate via a PHPUnit
        // <extensions> bootstrap (e.g. DAMADoctrineTestBundle, which wraps the
        // app's own Doctrine connection per test) genuinely need per-worker DBs
        // but carry NO RefreshDatabase/DatabaseTransactions trait, so `needs_db`
        // is false for them; skipping would silently drop parallel isolation and
        // the app would contend on one shared DB ("database is locked" at >1 worker).
        if !needs_db {
            eprintln!(
                "--provision-db: provisioning per-worker DBs on request (no marker-detected DB test; an <extensions> DB bridge may still need them)."
            );
        }
        let provision_script = proust::php_worker::find_provision_script()?;
        let run_uuid = format!("pr{}", std::process::id());
        // Compute every per-slot clone name up front and register them in the
        // lease BEFORE provisioning, so teardown (DROP IF EXISTS per name) stays
        // authoritative even if the batched provision crashes mid-CREATE.
        let clone_names: Vec<String> = (0..worker_count)
            .map(|slot| proust::resource_lease::clone_name(base, &run_uuid, slot))
            .collect();
        let mut lease = proust::resource_lease::ResourceLease::new(
            provision_script.clone(),
            autoload.clone(),
            bootstrap.clone(),
            defines.clone(),
            base.clone(),
        );
        for name in &clone_names {
            lease.register(name.clone());
        }
        // Batched provisioning: GC sweep + template + every per-slot clone in ONE
        // php spawn (was N+2 separate spawns: gc + build_template + N×clone).
        let provisioned = proust::resource_lease::provision_run(
            &provision_script,
            &autoload,
            bootstrap.as_deref(),
            &defines,
            base,
            &clone_names,
        )?;
        if provisioned.gc_dropped > 0 {
            eprintln!(
                "Resource provisioning: GC reclaimed {} stale clone(s) from a prior crashed run.",
                provisioned.gc_dropped
            );
        }
        eprintln!(
            "Resource provisioning: built template '{}' and {} per-slot clone(s).",
            provisioned.template,
            provisioned.dsns.len()
        );
        per_slot_dsn_opt = Some(provisioned.dsns);
        Some(proust::resource_lease::LeaseGuard::new(lease))
    } else {
        None
    };

    if single_process {
        eprintln!("Running tests in-process (no fork: single dispatch-safe worker)...");
    } else {
        eprintln!(
            "Spawning {} PHP worker{}...",
            worker_count,
            if worker_count == 1 { "" } else { "s" }
        );
    }
    let fork_script = find_fork_script()?;
    // Optional master-only warmup script (CLI --warmup wins over PROUST_WARMUP).
    // proust `require`s it ONCE in the fork master before forking; workers inherit
    // its warm state (loaded classes + shared opcache) via copy-on-write.
    let warmup_script: Option<PathBuf> = cli
        .warmup
        .clone()
        .or_else(|| std::env::var_os("PROUST_WARMUP").map(PathBuf::from));
    if let Some(w) = &warmup_script {
        if !w.is_file() {
            return Err(anyhow!("--warmup file not found: {}", w.display()));
        }
    }
    // Surface the warmup lever when it would help but isn't set: a functional
    // suite pays a cold framework-kernel boot per worker that --warmup elides.
    if is_functional_suite && warmup_script.is_none() {
        eprintln!(
            "proust: functional test suite detected (framework kernel boot). Consider \
             --warmup <file> to boot the kernel once in the master and cut the \
             per-worker cold-boot cost — see COMPATIBILITY.md \"Warmup hook\"."
        );
    }
    // L3: pick the no-fork master-inline spawn for an eligible tiny suite; identical 13-arg signature.
    let spawn_fn = if single_process {
        PhpForkPool::spawn_inline
    } else {
        PhpForkPool::spawn
    };
    let mut pool = profiler.span_with(
        "fork_pool_spawn",
        "main",
        serde_json::json!({"workers": worker_count, "inline": single_process}),
        || -> Result<_> {
            spawn_fn(
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
                per_slot_dsn_opt.as_deref(),
                warmup_script.as_deref(),
            )
        },
    )?;

    let stop_on = if cli.stop_on_defect {
        proust::runner::StopOn::on_defect()
    } else if cli.stop_on_failure {
        proust::runner::StopOn::on_failure()
    } else {
        proust::runner::StopOn::default()
    };
    // Keep clones for pool respawns; cfg takes ownership of the originals.
    let autoload_respawn = autoload.clone();
    let defines_respawn = defines.clone();
    let class_file_index_respawn = class_file_index.clone();
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
    profiler.mark("run_start", "main");
    const MAX_MASTER_RESPAWNS: u32 = 3;
    let mut all_outcomes: Vec<proust::types::TestOutcome> = Vec::new();
    let mut pending = cases;
    let mut master_respawns = 0u32;
    loop {
        let (partial, unfinished) = proust::runner::run_resumable(
            &mut pool,
            pending,
            &cfg,
            &row_counts,
            print_progress,
            &profiler,
        )?;
        all_outcomes.extend(partial.outcomes);
        if unfinished.is_empty() {
            break;
        }
        master_respawns += 1;
        if master_respawns > MAX_MASTER_RESPAWNS {
            eprintln!(
                "proust: PHP master died {} times; giving up on {} unfinished test(s)",
                master_respawns,
                unfinished.len()
            );
            for c in unfinished {
                let o = proust::types::TestOutcome {
                    class: c.class,
                    method: c.method,
                    dataset: None,
                    status: proust::types::TestStatus::Error,
                    message: Some("worker process crashed repeatedly; giving up".to_string()),
                    trace: None,
                    duration_ms: 0.0,
                };
                print_progress(&o);
                all_outcomes.push(o);
            }
            break;
        }
        eprintln!(
            "proust: PHP master died; respawning ({}/{}) for {} unfinished test(s)",
            master_respawns,
            MAX_MASTER_RESPAWNS,
            unfinished.len()
        );
        pool = spawn_fn(
            &fork_script,
            &autoload_respawn,
            bootstrap.as_deref(),
            &defines_respawn,
            &env_triples,
            &server_pairs,
            &ini_pairs,
            &var_pairs,
            worker_count,
            &class_file_index_respawn,
            &cli.worker_memory_limit,
            cli.worker_max_batches,
            per_slot_dsn_opt.as_deref(),
            warmup_script.as_deref(),
        )?;
        pending = unfinished;
    }
    profiler.mark("run_end", "main");
    let total_duration_ms: f64 = all_outcomes.iter().map(|o| o.duration_ms).sum();
    let mut report = proust::runner::Report {
        outcomes: all_outcomes,
        total_duration_ms,
    };
    let _ = n_cases;

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

    // Diagnostic: when PROUST_DUMP_TESTS is set, write one line per
    // executed test — one line PER DATA ROW — as `Class::method|Status|message`
    // (message: first 200 chars, newlines/pipes flattened; empty for passes).
    // This is the runner's exact expanded test list, which `--list-tests`
    // cannot provide (it is method-level). The bench's parity forensics
    // collapse it to per-method row counts, diff against vanilla's
    // `--list-tests`, and surface the messages of error outcomes — pinpointing
    // both WHICH tests diverge and WHY, on the machine where they diverge (the
    // CI-only parity drift). An env var rather than a CLI flag so wrappers
    // (bench_host.sh) pass it through without interface changes.
    if let Ok(dump_path) = std::env::var("PROUST_DUMP_TESTS") {
        if !dump_path.is_empty() {
            use std::fmt::Write as _;
            let mut buf = String::with_capacity(report.outcomes.len() * 64);
            for o in &report.outcomes {
                let msg: String = o
                    .message
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .map(|c| {
                        if c == '\n' || c == '\r' || c == '|' {
                            ' '
                        } else {
                            c
                        }
                    })
                    // 400 keeps the worker-death fatal suffix intact: a
                    // "Cannot redeclare" message carries two long absolute
                    // paths plus the registry/prewarm discriminator, which
                    // overflows 200 chars on deep suite trees (doctrine).
                    .take(400)
                    .collect();
                let _ = writeln!(buf, "{}::{}|{:?}|{}", o.class, o.method, o.status, msg);
            }
            if let Err(e) = std::fs::write(&dump_path, buf) {
                eprintln!("warning: PROUST_DUMP_TESTS write to {dump_path} failed: {e}");
            }
        }
    }

    print_summary(&report);

    // File-format reports (JUnit XML / TestDox). Written after the console summary
    // so a report-writing failure never masks the run result. Both consume only
    // the finished `report`, so they add zero cost when their flags are unset.
    if let Some(path) = &cli.log_junit {
        let xml = proust::reports::junit::junit_xml(&report, "");
        match std::fs::write(path, xml) {
            Ok(()) => eprintln!("JUnit XML written to {}", path.display()),
            Err(e) => eprintln!(
                "warning: writing JUnit XML to {} failed: {e}",
                path.display()
            ),
        }
    }
    if cli.testdox || cli.log_testdox_text.is_some() {
        let dox = proust::reports::testdox::testdox_text(&report);
        if cli.testdox {
            print!("\n{dox}");
        }
        if let Some(path) = &cli.log_testdox_text {
            match std::fs::write(path, &dox) {
                Ok(()) => eprintln!("TestDox written to {}", path.display()),
                Err(e) => eprintln!("warning: writing TestDox to {} failed: {e}", path.display()),
            }
        }
    }

    #[cfg(feature = "coverage")]
    if let Some(fmt) = &cli.coverage_format {
        use proust::coverage::{emit, passed_set};
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

    // Delegated runtime coverage: merge the per-worker .cov files and emit reports
    // via merge_coverage.php (php-code-coverage's own merge + writers). Best-effort —
    // a report failure warns but never fails the run whose results already printed.
    if let Some((cov_dir, cov_autoload)) = &cov_dir_guard {
        use proust::coverage_runtime::ReportTarget;
        let mut reports = Vec::new();
        if let Some(p) = &cli.coverage_clover {
            reports.push(ReportTarget {
                format: "clover".into(),
                target: Some(p.clone()),
            });
        }
        if let Some(p) = &cli.coverage_html {
            reports.push(ReportTarget {
                format: "html".into(),
                target: Some(p.clone()),
            });
        }
        if let Some(p) = &cli.coverage_text {
            let target = if p.as_os_str() == "-" {
                None
            } else {
                Some(p.clone())
            };
            reports.push(ReportTarget {
                format: "text".into(),
                target,
            });
        }
        match proust::php_worker::find_merge_coverage_script() {
            Ok(script) => match proust::coverage_runtime::merge_and_emit(
                &script,
                cov_autoload,
                cov_dir.path(),
                &reports,
            ) {
                Ok(()) => {
                    for r in &reports {
                        if let Some(t) = &r.target {
                            eprintln!("Coverage ({}) written to {}", r.format, t.display());
                        }
                    }
                }
                Err(e) => eprintln!("warning: coverage report failed: {e:#}"),
            },
            Err(e) => eprintln!("warning: merge_coverage.php not found: {e:#}"),
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
    use proust::types::TestCase;
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
            is_functional: false,
        }
    }

    #[test]
    fn gate_is_noop_when_nothing_needs_db() {
        let cases = vec![case("A", "t1", false), case("B", "t2", false)];
        assert!(
            !selected_needs_db(&cases),
            "no-DB suite must NOT trip the gate"
        );
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
        cases.retain(|c| proust::runner::matches_filter(&c.class, &c.method, Some(filter)));
        assert_eq!(cases.len(), 1);
        assert!(selected_needs_db(&cases), "filtered set still needs DB");
    }

    #[test]
    fn clamp_php_workers_caps_default_by_suite_size_but_honors_explicit() {
        // Default (not explicit): clamp to ceil(cases/K), floor 1.
        assert_eq!(clamp_php_workers(22, false, 12, 16), 1); // 12 tiny tests -> 1
        assert_eq!(clamp_php_workers(22, false, 100, 16), 7); // ceil(100/16)=7
        assert_eq!(clamp_php_workers(22, false, 2462, 16), 22); // faker: ceil=154 -> min(22,154)=22
        assert_eq!(clamp_php_workers(22, false, 0, 16), 1); // floor 1
                                                            // Explicit --workers N: honored verbatim, never clamped.
        assert_eq!(clamp_php_workers(8, true, 12, 16), 8);
    }

    #[test]
    fn provisioned_suites_clamp_to_fewer_workers_than_unit_suites() {
        // A provisioned (functional/DB) suite pays a high per-worker fixed cost,
        // so it uses the larger divisor and forks fewer workers than a unit suite
        // of the SAME size would.
        assert_eq!(worker_clamp_k(false), CLAMP_K_DEFAULT);
        assert_eq!(worker_clamp_k(true), CLAMP_K_PROVISIONED);
        // The measured case: a 53-test Symfony functional suite on a many-core
        // box. Default divisor over-forks to 4 (+5% vs vanilla); the provisioned
        // divisor lands on 2 (parity).
        assert_eq!(clamp_php_workers(22, false, 53, worker_clamp_k(false)), 4);
        assert_eq!(clamp_php_workers(22, false, 53, worker_clamp_k(true)), 2);
        // A large functional suite still scales out (no starvation): 320 cases.
        assert_eq!(clamp_php_workers(22, false, 320, worker_clamp_k(true)), 10);
        // Explicit --workers always wins, regardless of provisioning.
        assert_eq!(clamp_php_workers(8, true, 53, worker_clamp_k(true)), 8);
    }

    #[test]
    fn functional_suite_triggers_the_conservative_clamp_without_provision_db() {
        // A suite flagged functional (kernel boot) takes the same conservative
        // divisor as a provisioned one, even with no --provision-db.
        let unit = [case("A", "t", false)];
        assert!(!selected_is_functional(&unit));
        let mut fc = case("B", "t", false);
        fc.is_functional = true;
        let functional = [fc];
        assert!(selected_is_functional(&functional));
        // provision_db is absent here; the functional flag alone selects the
        // conservative divisor (the real call site ORs the two).
        assert_eq!(
            worker_clamp_k(selected_is_functional(&functional)),
            CLAMP_K_PROVISIONED
        );
        assert_eq!(
            worker_clamp_k(selected_is_functional(&unit)),
            CLAMP_K_DEFAULT
        );
    }

    #[test]
    fn single_process_eligible_only_for_one_worker_all_dispatch_safe() {
        let mk = |stateful: bool, isolated: bool, needs_db: bool| {
            let mut c = case("A", "t", needs_db);
            c.is_stateful = stateful;
            c.is_isolated = isolated;
            c
        };
        let safe = vec![mk(false, false, false), mk(false, false, false)];
        assert!(single_process_eligible(1, &safe));
        assert!(!single_process_eligible(2, &safe)); // >1 worker -> fork
        assert!(!single_process_eligible(1, &[mk(true, false, false)])); // stateful -> fork
        assert!(!single_process_eligible(1, &[mk(false, true, false)])); // isolated -> fork
        assert!(!single_process_eligible(1, &[mk(false, false, true)])); // needs_db -> fork
                                                                         // Size cap: a large all-dispatch-safe suite must NOT inline (no fork isolation means one
                                                                         // crash loses everything); it routes to the fork pool instead. Boundary at INLINE_MAX_CASES.
        let big: Vec<_> = (0..INLINE_MAX_CASES + 1)
            .map(|_| mk(false, false, false))
            .collect();
        assert!(!single_process_eligible(1, &big)); // >16 -> fork
        let at_cap: Vec<_> = (0..INLINE_MAX_CASES)
            .map(|_| mk(false, false, false))
            .collect();
        assert!(single_process_eligible(1, &at_cap)); // ==16 -> inline
    }

    #[test]
    fn autodetect_recognizes_symfony_phpunit_dist_xml() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Only the modern Symfony Flex spelling is present.
        std::fs::write(tmp.path().join("phpunit.dist.xml"), "<phpunit/>").unwrap();
        assert_eq!(
            autodetect_config_path(tmp.path()),
            Some(tmp.path().join("phpunit.dist.xml")),
            "phpunit.dist.xml (Symfony's layout) must be auto-detected"
        );
    }

    #[test]
    fn autodetect_config_precedence_committed_then_dist_spellings() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Legacy dist only.
        std::fs::write(tmp.path().join("phpunit.xml.dist"), "<phpunit/>").unwrap();
        assert_eq!(
            autodetect_config_path(tmp.path()),
            Some(tmp.path().join("phpunit.xml.dist"))
        );
        // Modern dist outranks legacy dist.
        std::fs::write(tmp.path().join("phpunit.dist.xml"), "<phpunit/>").unwrap();
        assert_eq!(
            autodetect_config_path(tmp.path()),
            Some(tmp.path().join("phpunit.dist.xml"))
        );
        // A committed phpunit.xml outranks every dist template.
        std::fs::write(tmp.path().join("phpunit.xml"), "<phpunit/>").unwrap();
        assert_eq!(
            autodetect_config_path(tmp.path()),
            Some(tmp.path().join("phpunit.xml"))
        );
    }

    #[test]
    fn autodetect_returns_none_when_no_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(autodetect_config_path(tmp.path()), None);
    }

    #[test]
    fn load_composer_psr4_map_parses_vendor_and_base_dir_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj = tmp.path();
        std::fs::create_dir_all(proj.join("vendor/composer")).unwrap();
        std::fs::write(
            proj.join("vendor/composer/autoload_psr4.php"),
            r#"<?php
$vendorDir = dirname(__DIR__);
$baseDir = dirname($vendorDir);
return array(
    'Psr\\Cache\\' => array($vendorDir . '/psr/cache/src'),
    'Faker\\Test\\' => array($baseDir . '/test/Faker'),
    'Faker\\' => array($baseDir . '/src/Faker'),
);
"#,
        )
        .unwrap();

        let map = load_composer_psr4_map(proj).expect("map should parse");
        assert!(map.contains(&(
            "Psr\\Cache\\".to_string(),
            proj.join("vendor/psr/cache/src")
        )));
        assert!(map.contains(&("Faker\\Test\\".to_string(), proj.join("test/Faker"))));
        assert!(map.contains(&("Faker\\".to_string(), proj.join("src/Faker"))));
    }

    #[test]
    fn load_composer_psr4_map_none_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(load_composer_psr4_map(tmp.path()).is_none());
    }

    #[test]
    fn is_psr4_resolvable_true_only_when_mapped_file_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj = tmp.path();
        std::fs::create_dir_all(proj.join("src/Faker/Provider")).unwrap();
        std::fs::write(proj.join("src/Faker/Provider/Lorem.php"), "<?php").unwrap();
        let map = vec![("Faker\\".to_string(), proj.join("src/Faker"))];

        assert!(is_psr4_resolvable("Faker\\Provider\\Lorem", &map));
        assert!(is_psr4_resolvable("\\Faker\\Provider\\Lorem", &map));
        assert!(!is_psr4_resolvable("Faker\\Provider\\Nope", &map));
        assert!(!is_psr4_resolvable("App\\Other", &map));
    }

    // Writes a minimal PSR-4-clean project: App\ -> src, App\Tests\ -> tests, with one test
    // class and one unreferenced src class. Returns (project_dir, roots, supplement_dirs).
    fn write_clean_fixture(tmp: &std::path::Path) -> (PathBuf, Vec<PathBuf>, Vec<PathBuf>) {
        std::fs::create_dir_all(tmp.join("vendor/composer")).unwrap();
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::create_dir_all(tmp.join("tests")).unwrap();
        std::fs::write(
            tmp.join("vendor/composer/autoload_psr4.php"),
            r#"<?php
$vendorDir = dirname(__DIR__);
$baseDir = dirname($vendorDir);
return array(
    'App\\Tests\\' => array($baseDir . '/tests'),
    'App\\' => array($baseDir . '/src'),
);
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.join("src/Helper.php"),
            "<?php\nnamespace App;\nclass Helper {}\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("tests/FooTest.php"),
            "<?php\nnamespace App\\Tests;\nuse PHPUnit\\Framework\\TestCase;\nclass FooTest extends TestCase {\n  public function testBar() { $this->assertTrue(true); }\n}\n",
        )
        .unwrap();
        (
            tmp.to_path_buf(),
            vec![tmp.join("tests")],
            vec![tmp.join("tests"), tmp.join("src")],
        )
    }

    #[test]
    fn build_cases_and_index_fast_path_skips_src_when_psr4_clean() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (proj, roots, supp) = write_clean_fixture(tmp.path());

        let (cases, index) = build_cases_and_index(&proj, &roots, &[], &supp, false).unwrap();
        let (full_cases, _full_index) =
            proust::discovery::discover_with_index(&roots, &[], &supp).unwrap();
        assert_eq!(cases.len(), full_cases.len());
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].class, "App\\Tests\\FooTest");
        assert!(!index.contains_key("App\\Helper"));
    }

    #[test]
    fn build_cases_and_index_full_path_when_need_full_index() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (proj, roots, supp) = write_clean_fixture(tmp.path());

        let (cases, index) = build_cases_and_index(&proj, &roots, &[], &supp, true).unwrap();
        let (full_cases, full_index) =
            proust::discovery::discover_with_index(&roots, &[], &supp).unwrap();
        assert_eq!(cases.len(), full_cases.len());
        assert_eq!(index, full_index);
        assert!(index.contains_key("App\\Helper"));
    }

    #[test]
    fn build_cases_and_index_falls_back_when_external_provider_not_psr4() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj = tmp.path();
        std::fs::create_dir_all(proj.join("vendor/composer")).unwrap();
        std::fs::create_dir_all(proj.join("tests")).unwrap();
        std::fs::create_dir_all(proj.join("fixtures")).unwrap();
        // PSR-4 maps App\Tests\ -> tests ONLY; App\Fixtures\ is NOT mapped.
        std::fs::write(
            proj.join("vendor/composer/autoload_psr4.php"),
            r#"<?php
$vendorDir = dirname(__DIR__);
$baseDir = dirname($vendorDir);
return array(
    'App\\Tests\\' => array($baseDir . '/tests'),
);
"#,
        )
        .unwrap();
        // Provider class at a non-PSR-4 path (filename deliberately differs from class name).
        std::fs::write(
            proj.join("fixtures/data_rows.php"),
            "<?php\nnamespace App\\Fixtures;\nclass Data { public static function rows() { return [[1]]; } }\n",
        )
        .unwrap();
        std::fs::write(
            proj.join("tests/FooTest.php"),
            "<?php\nnamespace App\\Tests;\nuse PHPUnit\\Framework\\TestCase;\nuse PHPUnit\\Framework\\Attributes\\DataProviderExternal;\nclass FooTest extends TestCase {\n  #[DataProviderExternal(\\App\\Fixtures\\Data::class, 'rows')]\n  public function testBar($x) { $this->assertTrue((bool)$x); }\n}\n",
        )
        .unwrap();

        let roots = vec![proj.join("tests")];
        let supp = vec![proj.join("tests"), proj.join("fixtures")];
        let (_cases, index) = build_cases_and_index(proj, &roots, &[], &supp, false).unwrap();
        assert!(
            index.contains_key("App\\Fixtures\\Data"),
            "non-PSR-4 provider must be kept via the full-parse fallback (parity)"
        );
    }

    #[test]
    fn build_cases_and_index_fallback_index_matches_full_parse() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj = tmp.path();
        std::fs::create_dir_all(proj.join("vendor/composer")).unwrap();
        std::fs::create_dir_all(proj.join("tests")).unwrap();
        std::fs::create_dir_all(proj.join("fixtures")).unwrap();
        std::fs::write(
            proj.join("vendor/composer/autoload_psr4.php"),
            "<?php\n$vendorDir = dirname(__DIR__);\n$baseDir = dirname($vendorDir);\nreturn array(\n    'App\\\\Tests\\\\' => array($baseDir . '/tests'),\n);\n",
        )
        .unwrap();
        std::fs::write(
            proj.join("fixtures/data_rows.php"),
            "<?php\nnamespace App\\Fixtures;\nclass Data { public static function rows() { return [[1]]; } }\n",
        )
        .unwrap();
        std::fs::write(
            proj.join("tests/FooTest.php"),
            "<?php\nnamespace App\\Tests;\nuse PHPUnit\\Framework\\TestCase;\nuse PHPUnit\\Framework\\Attributes\\DataProviderExternal;\nclass FooTest extends TestCase {\n  #[DataProviderExternal(\\App\\Fixtures\\Data::class, 'rows')]\n  public function testBar($x) { $this->assertTrue((bool)$x); }\n}\n",
        )
        .unwrap();
        let roots = vec![proj.join("tests")];
        let supp = vec![proj.join("tests"), proj.join("fixtures")];

        let (_c1, fallback_index) = build_cases_and_index(proj, &roots, &[], &supp, false).unwrap();
        let (_c2, full_index) = proust::discovery::discover_with_index(&roots, &[], &supp).unwrap();
        assert_eq!(
            fallback_index, full_index,
            "fallback index must byte-match discover_with_index"
        );
        assert!(fallback_index.contains_key("App\\Fixtures\\Data"));
    }
}
