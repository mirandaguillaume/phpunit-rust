//! `pcov-rs analyze` — run static coverage analysis and emit results.

use crate::analyzer::{self, Coverage};
use crate::boundary::BoundaryResolver;
use crate::cache::{CacheStore, ContentHash};
use crate::config::{self, ConfigError};
use crate::mago_bridge::MagoProject;
use crate::output::{render, Format};
use crate::test_discovery::{self, TestMethod};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    pub common: super::CommonOpts,

    /// Output format: pcov | pcov-extended | clover | json
    #[arg(long, default_value = "pcov-extended")]
    pub format: String,

    /// Write output to file (default: stdout)
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Only analyze test methods matching this glob pattern (Phase 2 — currently ignored)
    #[arg(long)]
    pub filter: Option<String>,

    /// Bypass cache for this run
    #[arg(long)]
    pub no_cache: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct FileMeta {
    size: u64,
    mtime_nanos: u64,
}

/// Per-test trace result with dependency snapshots for incremental invalidation.
/// Cached entry is still valid when every dep path's size+mtime matches current disk state.
#[derive(serde::Serialize, serde::Deserialize)]
struct TraceCacheEntry {
    coverage: Coverage,
    /// Files this test's trace depended on, snapshotted at trace time.
    dep_snapshots: HashMap<PathBuf, FileMeta>,
}

impl TraceCacheEntry {
    fn still_valid(&self, current: &HashMap<PathBuf, FileMeta>) -> bool {
        self.dep_snapshots.iter().all(|(p, s)| {
            current
                .get(p)
                .is_some_and(|m| m.size == s.size && m.mtime_nanos == s.mtime_nanos)
        })
    }
}

/// Fully-merged and proxy-applied coverage result. A hit means zero PHP load or trace work.
#[derive(serde::Serialize, serde::Deserialize)]
struct ResultCacheEntry {
    coverage: Coverage,
}

/// Run the full analysis pipeline with an optional test filter.
///
/// `allowed` — if `Some`, only test methods whose `(class, method)` pair is in
/// the set are traced. Pass `None` to trace all discovered tests (the
/// default for the standalone `pcov-rs analyze` command).
///
/// Returns the merged coverage map. Skips the result cache when `allowed` is
/// `Some` (the cache key doesn't encode the filter).
///
/// The per-test trace cache is always consulted (no `no_cache` escape hatch).
/// Stale entries are invalidated automatically via file-mtime snapshots.
/// Run analysis work on a rayon pool with a large stack.
///
/// mago's recursive-descent parser (invoked by `MagoProject::load`) and the
/// type walker recurse with depth proportional to PHP expression/statement
/// nesting; on a default ~2–8 MB stack, deeply-nested source (long boolean
/// chains, nested array literals) can overflow and **abort the whole process**
/// — observed crashing inside `load()` at ~30–40 levels on a 2 MB stack.
/// Running the work inside a pool with a generous stack raises the safe
/// nesting bound far beyond any realistic code. The nested `par_iter` in
/// `analyze_filtered_inner` runs on this same pool, so the per-test walk
/// workers inherit the large stack too.
pub fn with_deep_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    const DEEP_STACK: usize = 128 * 1024 * 1024;
    match rayon::ThreadPoolBuilder::new()
        .stack_size(DEEP_STACK)
        .build()
    {
        Ok(pool) => pool.install(f),
        Err(_) => f(), // fall back to the current thread if pool creation fails
    }
}

pub fn analyze_filtered(
    cfg: &crate::config::ProjectConfig,
    allowed: Option<&std::collections::HashSet<(String, String)>>,
) -> anyhow::Result<crate::analyzer::Coverage> {
    // Tier-1 result cache: a hit needs no parsing or walking, so probe it on
    // the normal stack and skip building the (expensive) deep-stack worker
    // pool. Only an actual parse/trace (a miss) enters `with_deep_stack`.
    if allowed.is_none() {
        let cache = CacheStore::open(&cfg.root, MagoProject::version())?;
        let file_metas = collect_file_metas(&cfg.source_includes, &cfg.test_suites);
        let fingerprint = fingerprint_from_metas(config_fingerprint(cfg).as_str(), &file_metas);
        if let Ok(Some(e)) = cache.get::<ResultCacheEntry>("result", &fingerprint) {
            return Ok(e.coverage);
        }
    }
    with_deep_stack(|| analyze_filtered_inner(cfg, allowed))
}

fn analyze_filtered_inner(
    cfg: &crate::config::ProjectConfig,
    allowed: Option<&std::collections::HashSet<(String, String)>>,
) -> anyhow::Result<crate::analyzer::Coverage> {
    let boundary = BoundaryResolver::from_config(cfg);
    let cache = CacheStore::open(&cfg.root, MagoProject::version())?;
    let cfg_fp = config_fingerprint(cfg);

    let mut test_files = Vec::new();
    for suite in &cfg.test_suites {
        collect_php_files(suite, &mut test_files);
    }

    let file_metas = collect_file_metas(&cfg.source_includes, &cfg.test_suites);

    // Tier 1 result cache: only valid when no filter is applied.
    if allowed.is_none() {
        let fingerprint = fingerprint_from_metas(cfg_fp.as_str(), &file_metas);
        if let Ok(Some(e)) = cache.get::<ResultCacheEntry>("result", &fingerprint) {
            return Ok(e.coverage);
        }
    }

    // Tier 2/3: per-test trace cache.
    let trace_check = check_trace_caches(&cache, cfg_fp.as_str(), &test_files, &file_metas);
    let (project, tests, cached_traces) = match trace_check {
        Some((tests, traces)) => {
            let all_valid = traces.iter().all(Option::is_some);
            let project = if all_valid {
                MagoProject::load_excluding_vendor(&cfg.root)?
            } else {
                MagoProject::load(&cfg.root)?
            };
            (project, tests, traces)
        }
        None => {
            let project = MagoProject::load(&cfg.root)?;
            let tests = test_discovery::discover(&project, &cache, &test_files)?;
            let n = tests.len();
            (project, tests, vec![None::<crate::analyzer::Coverage>; n])
        }
    };

    let all_coverages: Vec<crate::analyzer::Coverage> = {
        use rayon::prelude::*;
        tests
            .par_iter()
            .zip(cached_traces.into_par_iter())
            .filter_map(|(test, cached_cov)| {
                if let Some(set) = allowed {
                    if !set.contains(&(test.class.clone(), test.method.clone())) {
                        return None;
                    }
                }
                Some(cached_cov.unwrap_or_else(|| {
                    trace_and_cache(
                        &project,
                        &boundary,
                        cfg_fp.as_str(),
                        test,
                        &file_metas,
                        &cache,
                        false,
                    )
                }))
            })
            .collect()
    };
    let mut coverage: crate::analyzer::Coverage = HashMap::new();
    for cov in all_coverages {
        analyzer::merge(&mut coverage, cov);
    }

    analyzer::proxy::add_proxy_coverage(&project, &boundary, &mut coverage);

    // Report-time source filter — drop anything not under the configured
    // <source> includes. The tracer records the entry point (each test method's
    // own file), so without this the report leaks test files that PHPUnit, which
    // reports only <source>, never includes.
    restrict_to_source(&mut coverage, &boundary, !cfg.source_includes.is_empty());

    // Store result cache only for unfiltered runs.
    if allowed.is_none() {
        let fingerprint = fingerprint_from_metas(cfg_fp.as_str(), &file_metas);
        let _ = cache.put(
            "result",
            &fingerprint,
            &ResultCacheEntry {
                coverage: coverage.clone(),
            },
        );
    }

    Ok(coverage)
}

/// Keep only files under the configured `<source>` includes (`Boundary::Project`).
/// PHPUnit reports coverage only for `<source>` — never for test files — but the
/// tracer's entry point is each test method's own file, so those leak into the
/// map. A no-op when no `<source>` is configured (empty includes), so an
/// unconfigured project still gets whole-project coverage instead of an empty
/// report.
fn restrict_to_source(
    coverage: &mut Coverage,
    boundary: &BoundaryResolver,
    has_source_config: bool,
) {
    if !has_source_config {
        return;
    }
    coverage.retain(|path, _| boundary.classify(path) == crate::boundary::Boundary::Project);
}

#[cfg(test)]
mod source_filter_tests {
    use super::*;
    use crate::config::ProjectConfig;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "<?php").unwrap();
    }

    #[test]
    fn drops_test_files_keeps_src() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src/Calc.php");
        let test = dir.path().join("tests/CalcTest.php");
        touch(&src);
        touch(&test);
        let cfg = ProjectConfig {
            root: dir.path().to_path_buf(),
            test_suites: vec![dir.path().join("tests")],
            source_includes: vec![dir.path().join("src")],
            source_excludes: vec![],
        };
        let boundary = BoundaryResolver::from_config(&cfg);
        let mut cov: Coverage = HashMap::new();
        cov.insert(src.canonicalize().unwrap(), HashMap::new());
        cov.insert(test.canonicalize().unwrap(), HashMap::new());

        restrict_to_source(&mut cov, &boundary, true);

        assert!(cov.keys().any(|p| p.ends_with("Calc.php")), "src file kept");
        assert!(
            !cov.keys().any(|p| p.ends_with("CalcTest.php")),
            "test file dropped (PHPUnit reports only <source>)"
        );
    }

    #[test]
    fn is_noop_without_source_config() {
        let dir = tempfile::tempdir().unwrap();
        let test = dir.path().join("tests/CalcTest.php");
        touch(&test);
        let cfg = ProjectConfig {
            root: dir.path().to_path_buf(),
            test_suites: vec![dir.path().join("tests")],
            source_includes: vec![],
            source_excludes: vec![],
        };
        let boundary = BoundaryResolver::from_config(&cfg);
        let mut cov: Coverage = HashMap::new();
        cov.insert(test.canonicalize().unwrap(), HashMap::new());

        restrict_to_source(&mut cov, &boundary, false);

        assert_eq!(cov.len(), 1, "no <source> config -> keep everything");
    }
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
    let cache = CacheStore::open(&cfg.root, MagoProject::version())?;
    let cfg_fp = config_fingerprint(&cfg);

    let mut test_files = Vec::new();
    for suite in &cfg.test_suites {
        collect_php_files(suite, &mut test_files);
    }

    let file_metas = collect_file_metas(&cfg.source_includes, &cfg.test_suites);
    let fingerprint = fingerprint_from_metas(cfg_fp.as_str(), &file_metas);

    // Tier 1: fully-baked result cache — no PHP load, no trace work.
    if !args.no_cache {
        if let Ok(Some(e)) = cache.get::<ResultCacheEntry>("result", &fingerprint) {
            return emit(e.coverage, &args);
        }
    }

    // Tier 2/3 + load + trace: mago's parser and the type walker recurse with
    // PHP nesting depth, so this work runs on a large-stack rayon pool (deeply
    // nested source would otherwise overflow). The tier-1 hit above returns
    // before we get here, so a warm "nothing changed" run never pays for the
    // pool — only an actual parse/trace does.
    with_deep_stack(move || -> anyhow::Result<()> {
        // Tier 2/3: check which per-test trace entries are still valid.
        let trace_check = if !args.no_cache {
            check_trace_caches(&cache, cfg_fp.as_str(), &test_files, &file_metas)
        } else {
            None
        };

        // Load project: skip vendor when every trace is already valid.
        let (project, tests, cached_traces) = match trace_check {
            Some((tests, traces)) => {
                let all_valid = traces.iter().all(Option::is_some);
                let project = if all_valid {
                    MagoProject::load_excluding_vendor(&cfg.root)?
                } else {
                    MagoProject::load(&cfg.root)?
                };
                (project, tests, traces)
            }
            None => {
                let project = MagoProject::load(&cfg.root)?;
                let tests = test_discovery::discover(&project, &cache, &test_files)?;
                let n = tests.len();
                (project, tests, vec![None::<Coverage>; n])
            }
        };

        // Per-test coverage: reuse valid cached entries; re-trace stale ones in parallel.
        let all_coverages: Vec<Coverage> = {
            use rayon::prelude::*;
            let no_cache = args.no_cache;
            tests
                .par_iter()
                .zip(cached_traces.into_par_iter())
                .map(|(test, cached_cov)| {
                    cached_cov.unwrap_or_else(|| {
                        trace_and_cache(
                            &project,
                            &boundary,
                            cfg_fp.as_str(),
                            test,
                            &file_metas,
                            &cache,
                            no_cache,
                        )
                    })
                })
                .collect()
        };
        let mut coverage: Coverage = HashMap::new();
        for cov in all_coverages {
            analyzer::merge(&mut coverage, cov);
        }

        analyzer::proxy::add_proxy_coverage(&project, &boundary, &mut coverage);

        // Store the baked result so the next run hits tier 1.
        if !args.no_cache {
            let _ = cache.put(
                "result",
                &fingerprint,
                &ResultCacheEntry {
                    coverage: coverage.clone(),
                },
            );
        }

        emit(coverage, &args)
    })
}

/// Check discovery + per-test trace caches. Returns `None` on any discovery miss.
/// For each test, `Some(coverage)` = still valid; `None` = must re-trace.
fn check_trace_caches(
    cache: &CacheStore,
    cfg_fp: &str,
    test_files: &[PathBuf],
    file_metas: &HashMap<PathBuf, FileMeta>,
) -> Option<(Vec<TestMethod>, Vec<Option<Coverage>>)> {
    let tests = test_discovery::try_from_cache(cache, test_files).ok()??;
    let traces = tests
        .iter()
        .map(|t| {
            let key = trace_v2_key(cfg_fp, &t.class, &t.method);
            match cache.get::<TraceCacheEntry>("trace_v2", &key) {
                Ok(Some(e)) if e.still_valid(file_metas) => Some(e.coverage),
                _ => None,
            }
        })
        .collect();
    Some((tests, traces))
}

/// Trace all data-provider expansions of `test`, write a `trace_v2` cache entry, return the coverage.
fn trace_and_cache(
    project: &MagoProject,
    boundary: &BoundaryResolver,
    cfg_fp: &str,
    test: &TestMethod,
    file_metas: &HashMap<PathBuf, FileMeta>,
    cache: &CacheStore,
    no_cache: bool,
) -> Coverage {
    let mut test_cov: Coverage = HashMap::new();
    for expanded in analyzer::data_provider::expand(project, test) {
        let t = analyzer::trace::trace_test(project, boundary, &expanded.test, expanded.data_set);
        analyzer::merge(&mut test_cov, t);
    }

    if !no_cache {
        // Files covered by this test + the test's own file are its dependencies.
        let mut dep_snapshots: HashMap<PathBuf, FileMeta> = test_cov
            .keys()
            .filter_map(|p| file_metas.get(p).map(|m| (p.clone(), m.clone())))
            .collect();
        if let Some(m) = file_metas.get(&test.file) {
            dep_snapshots
                .entry(test.file.clone())
                .or_insert_with(|| m.clone());
        }
        let entry = TraceCacheEntry {
            coverage: test_cov.clone(),
            dep_snapshots,
        };
        let _ = cache.put(
            "trace_v2",
            &trace_v2_key(cfg_fp, &test.class, &test.method),
            &entry,
        );
    }

    test_cov
}

/// Stable fingerprint of the resolved project config's source boundaries.
///
/// `opacity::decide` routes Trace-vs-Opaque off `source_includes` /
/// `source_excludes`, so editing those (or pointing `--config` at a file with
/// different boundaries) changes the coverage map with NO PHP-file mtime
/// change. Folding this fingerprint into the derived cache keys ensures any
/// boundary change invalidates the trace_v2 and result caches.
fn config_fingerprint(cfg: &crate::config::ProjectConfig) -> ContentHash {
    ContentHash::of_config(
        &cfg.root,
        &cfg.source_includes,
        &cfg.source_excludes,
        &cfg.test_suites,
    )
}

fn trace_v2_key(cfg_fp: &str, class: &str, method: &str) -> ContentHash {
    ContentHash::of_bytes(format!("trace_v2\x00{cfg_fp}\x00{class}\x00{method}").as_bytes())
}

fn emit(coverage: Coverage, args: &Args) -> anyhow::Result<()> {
    let format: Format = args.format.parse().map_err(anyhow::Error::msg)?;
    let rendered = render(format, &coverage);
    if let Some(path) = &args.output {
        std::fs::write(path, rendered)?;
    } else {
        println!("{rendered}");
    }
    Ok(())
}

/// Build a flat map of all PHP files under the given dirs with their (size, mtime) snapshots.
fn collect_file_metas(
    source_includes: &[PathBuf],
    test_suites: &[PathBuf],
) -> HashMap<PathBuf, FileMeta> {
    let mut metas = HashMap::new();
    for dir in source_includes.iter().chain(test_suites.iter()) {
        collect_php_file_metas_rec(dir, &mut metas);
    }
    metas
}

fn collect_php_file_metas_rec(dir: &Path, out: &mut HashMap<PathBuf, FileMeta>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_php_file_metas_rec(&path, out);
            } else if path.extension().is_some_and(|e| e == "php") {
                if let Ok(meta) = std::fs::metadata(&path) {
                    let mtime_nanos = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    out.insert(
                        path,
                        FileMeta {
                            size: meta.len(),
                            mtime_nanos,
                        },
                    );
                }
            }
        }
    }
}

/// BLAKE3 fingerprint of the config boundary plus sorted (path, size, mtime)
/// for all tracked PHP files. Any file change (add, edit, delete) — or any
/// source-boundary change (`cfg_fp`) — produces a new hash, invalidating the
/// result cache. `cfg_fp` must be the hex output of [`config_fingerprint`].
fn fingerprint_from_metas(cfg_fp: &str, metas: &HashMap<PathBuf, FileMeta>) -> ContentHash {
    let mut paths: Vec<&PathBuf> = metas.keys().collect();
    paths.sort();
    let mut buf = Vec::new();
    buf.extend_from_slice(b"result\x00");
    buf.extend_from_slice(cfg_fp.as_bytes());
    buf.push(0);
    for p in paths {
        buf.extend_from_slice(p.to_string_lossy().as_bytes());
        buf.push(0);
        let m = &metas[p];
        buf.extend_from_slice(&m.size.to_le_bytes());
        buf.extend_from_slice(&m.mtime_nanos.to_le_bytes());
    }
    ContentHash::of_bytes(&buf)
}

fn collect_php_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_php_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "php") {
                out.push(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectConfig;

    fn cfg_with(includes: Vec<PathBuf>, excludes: Vec<PathBuf>) -> ProjectConfig {
        ProjectConfig {
            root: PathBuf::from("/proj"),
            test_suites: vec![PathBuf::from("/proj/tests")],
            source_includes: includes,
            source_excludes: excludes,
        }
    }

    /// Two configs over the SAME files on disk but with different source
    /// boundaries must produce different derived cache keys, otherwise editing
    /// `<source><include>/<exclude>` returns stale coverage (bug H5).
    #[test]
    fn config_fingerprint_differs_on_boundary_change() {
        let a = cfg_with(vec![PathBuf::from("/proj/src")], vec![]);
        let b = cfg_with(
            vec![PathBuf::from("/proj/src")],
            vec![PathBuf::from("/proj/src/Migrations")],
        );
        assert_ne!(
            config_fingerprint(&a),
            config_fingerprint(&b),
            "adding a source <exclude> must change the config fingerprint",
        );

        let c = cfg_with(vec![PathBuf::from("/proj/app")], vec![]);
        assert_ne!(
            config_fingerprint(&a),
            config_fingerprint(&c),
            "changing a source <include> must change the config fingerprint",
        );
    }

    #[test]
    fn config_fingerprint_same_for_identical_config() {
        let a = cfg_with(vec![PathBuf::from("/proj/src")], vec![]);
        let b = cfg_with(vec![PathBuf::from("/proj/src")], vec![]);
        assert_eq!(config_fingerprint(&a), config_fingerprint(&b));
    }

    /// The per-test trace_v2 key must be namespaced by config so that a
    /// boundary change invalidates the per-test trace cache for the same
    /// (class, method).
    #[test]
    fn trace_v2_key_namespaced_by_config() {
        let a = config_fingerprint(&cfg_with(vec![PathBuf::from("/proj/src")], vec![]));
        let b = config_fingerprint(&cfg_with(
            vec![PathBuf::from("/proj/src")],
            vec![PathBuf::from("/proj/src/Migrations")],
        ));
        assert_ne!(
            trace_v2_key(a.as_str(), "FooTest", "testBar"),
            trace_v2_key(b.as_str(), "FooTest", "testBar"),
            "trace_v2 key must change when the config boundary changes",
        );
        // Same config, same (class, method) => same key.
        assert_eq!(
            trace_v2_key(a.as_str(), "FooTest", "testBar"),
            trace_v2_key(a.as_str(), "FooTest", "testBar"),
        );
    }

    /// The tier-1 result fingerprint must be namespaced by config so that a
    /// boundary change invalidates the baked result cache even when no PHP
    /// file mtime changed.
    #[test]
    fn result_fingerprint_namespaced_by_config() {
        let metas: HashMap<PathBuf, FileMeta> = HashMap::new();
        let a = config_fingerprint(&cfg_with(vec![PathBuf::from("/proj/src")], vec![]));
        let b = config_fingerprint(&cfg_with(
            vec![PathBuf::from("/proj/src")],
            vec![PathBuf::from("/proj/src/Migrations")],
        ));
        assert_ne!(
            fingerprint_from_metas(a.as_str(), &metas),
            fingerprint_from_metas(b.as_str(), &metas),
            "result fingerprint must change when the config boundary changes",
        );
        assert_eq!(
            fingerprint_from_metas(a.as_str(), &metas),
            fingerprint_from_metas(a.as_str(), &metas),
        );
    }
}
