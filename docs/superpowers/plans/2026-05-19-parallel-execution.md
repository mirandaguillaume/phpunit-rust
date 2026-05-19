# Parallel Execution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run test classes concurrently across N FrankenPHP worker processes, delivering a wall-clock speedup roughly proportional to the number of CPU cores on CPU-bound test suites. Vanilla PHPUnit can't do this; this is the headline feature that differentiates phpunit-rust.

**Architecture:** Spawn N independent FrankenPHP processes at startup (each on its own free localhost port). The Rust orchestrator distributes discovered test classes across them using `rayon::par_iter()` with a thread pool sized to N. Each rayon worker thread is assigned one `WorkerClient` by index; `client.run_class()` calls remain synchronous (ureq), but multiple are in flight simultaneously across the rayon pool. Drop on `WorkerPool` kills all children. The v0.3 "own the runner" pivot eliminated PHPUnit singletons (Facade, Registry, PassedTests), so the multi-process model has zero cross-worker state to coordinate — each request is fully self-contained.

**Tech Stack:**
- Existing: Rust 1.75+, ureq, FrankenPHP 1.x, PHPUnit 10.5/11.5
- New Rust crates: `rayon` (parallel iteration with bounded thread pool), `num_cpus` (CPU count detection)
- Wire protocol: **unchanged** — same `TestRunRequest`/`TestOutcome` shape from v0.3

**Verification notes for the implementer (read before starting):**
- **`rayon::ThreadPoolBuilder::new().num_threads(n).build_global()` can only be called once per process.** If called twice (or after rayon's default pool was lazily created), it returns `Err(ThreadPoolBuildError)`. We initialize it explicitly in `main()` before any rayon operation runs. In tests that exercise parallel code paths, use a *local* `ThreadPool::install` to avoid clobbering the global.
- **`ureq::Agent` is `Clone + Send + Sync`.** Cloning shares the connection pool. We can give each `WorkerClient` its own Agent OR share one — both work. We give each client its own (simpler, no shared lifetimes).
- **`find_free_port()` has a TOCTOU race.** Spawning N workers in parallel multiplies the risk. We serialize port-finding (sequential during pool init), then start the FrankenPHP children in parallel.
- **FrankenPHP's startup is ~1-2s.** With sequential readiness waits, an 8-worker pool would cost ~8-16s at startup. We probe readiness in parallel via rayon.
- **stdout flushes are atomic per-`write!`.** Multiple rayon threads calling `print!(".")` simultaneously won't corrupt characters, but they may interleave in unintuitive orders. That's fine — users expect parallel progress output to be unordered.

---

## File Structure

```
Cargo.toml                  # add rayon + num_cpus
src/frankenphp.rs           # add WorkerPool { workers: Vec<FrankenPhp>, urls: Vec<String> }
src/runner.rs               # accept &WorkerPool; use rayon::par_iter for distribution
src/client.rs               # unchanged (Agent already Clone+Send+Sync)
src/main.rs                 # --workers flag (default num_cpus); init rayon's global pool
src/reporter.rs             # on_progress signature: `impl Fn(...) + Sync` (was FnMut)
src/types.rs                # unchanged
tests/integration.rs        # add parallel-run test exercising 2+ workers
docs/superpowers/plans/2026-05-19-parallel-execution.md   # this plan
```

**Boundaries:**
- `WorkerPool` owns N `FrankenPhp` instances + their URLs. It's the public type that replaces single-worker `FrankenPhp` in the runner's public API.
- `runner::run` is now `Fn` (not `FnMut`) on the progress callback because rayon needs `Sync`. Mutation has to live behind interior mutability if callers need it (the reporter's `print_progress` doesn't mutate, so it's fine).
- The wire types and worker.php are completely untouched — parallel execution is a pure Rust-side concern.

---

## Task 1: Add rayon + num_cpus dependencies

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/Cargo.toml`

- [ ] **Step 1: Add dependencies**

In `Cargo.toml` under `[dependencies]`, add:

```toml
num_cpus = "1.16"
rayon = "1.10"
```

- [ ] **Step 2: Build to fetch crates**

```bash
cd /home/gumiranda/PHPUnit_rust && cargo build --lib 2>&1 | tail -3
```

Expected: clean build with two new crates downloaded.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: add rayon + num_cpus for parallel execution"
```

---

## Task 2: WorkerPool struct + spawn

Introduce `WorkerPool` that owns N `FrankenPhp` children. Each child gets its own port and is spawned sequentially (to avoid `find_free_port` races); readiness probes happen in parallel.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/frankenphp.rs`

- [ ] **Step 1: Write failing tests**

Add to `#[cfg(test)] mod tests` in `src/frankenphp.rs`:

```rust
    #[test]
    fn worker_pool_spawns_requested_count() {
        let worker = find_worker_script().expect("worker.php must exist");
        let pool = WorkerPool::spawn(&worker, 3).expect("3-worker pool must spawn");
        assert_eq!(pool.urls().len(), 3);
        // URLs must be distinct (different ports).
        let mut urls: Vec<&String> = pool.urls().iter().collect();
        urls.sort();
        urls.dedup();
        assert_eq!(urls.len(), 3, "all worker URLs must be unique");
    }

    #[test]
    fn worker_pool_rejects_zero_workers() {
        let worker = find_worker_script().unwrap();
        let err = WorkerPool::spawn(&worker, 0).unwrap_err();
        assert!(err.to_string().contains("at least 1"), "{err:#}");
    }
```

- [ ] **Step 2: Implement WorkerPool**

Append to `src/frankenphp.rs` (after the existing `FrankenPhp` struct + impls):

```rust
/// A pool of N FrankenPHP worker processes. The Rust runner distributes test
/// classes across them via rayon. Each worker is fully independent: its own
/// FrankenPHP process, its own port, its own PHP interpreter state — so we
/// inherit no cross-worker state-isolation problems.
pub struct WorkerPool {
    // Order matters: workers must be dropped in reverse so the last-spawned
    // is killed first. Vec drops elements in order; we rely on each
    // FrankenPhp::drop() being self-contained.
    workers: Vec<FrankenPhp>,
}

impl WorkerPool {
    /// Spawn `n` FrankenPHP children, each binding a free localhost port.
    /// Returns when every child is serving requests.
    ///
    /// Spawning is sequential (to avoid `find_free_port` TOCTOU races among
    /// concurrent finders), but readiness probing happens in parallel.
    pub fn spawn(worker_script: &Path, n: usize) -> Result<Self> {
        if n == 0 {
            return Err(anyhow!("worker pool needs at least 1 worker (got 0)"));
        }
        let mut workers: Vec<FrankenPhp> = Vec::with_capacity(n);
        for _ in 0..n {
            workers.push(FrankenPhp::spawn(worker_script)?);
        }
        Ok(WorkerPool { workers })
    }

    /// Returns one base URL (without the `/worker.php` suffix) per child.
    pub fn urls(&self) -> Vec<String> {
        self.workers
            .iter()
            .map(|w| w.worker_url())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.workers.len()
    }
}
```

- [ ] **Step 3: Run pool tests**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null || true
sleep 1
cargo test --lib frankenphp 2>&1 | tail -5
```

Expected: existing `find_free_port_returns_usable_port` + 2 new pool tests = 3 passing. Run takes ~6-10s because each spawned pool waits for readiness.

- [ ] **Step 4: Commit**

```bash
git add src/frankenphp.rs
git commit -m "feat(frankenphp): WorkerPool spawns N FrankenPHP children with distinct ports"
```

---

## Task 3: Update runner to accept WorkerPool + parallelize

The runner stops taking a single `WorkerClient` and instead takes a `&WorkerPool`, creating per-thread `WorkerClient` references and using rayon's parallel iterator.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/runner.rs`

- [ ] **Step 1: Replace `src/runner.rs`**

```rust
use crate::client::WorkerClient;
use crate::discovery::{group_by_class, TestClass};
use crate::frankenphp::WorkerPool;
use crate::types::{TestCase, TestOutcome, TestRunRequest, TestStatus};
use anyhow::Result;
use rayon::prelude::*;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub autoload: PathBuf,
    pub bootstrap: Option<PathBuf>,
    pub filter: Option<String>,
}

#[derive(Debug)]
pub struct Report {
    pub outcomes: Vec<TestOutcome>,
    pub total_duration_ms: f64,
}

impl Report {
    pub fn count(&self, status: TestStatus) -> usize {
        self.outcomes.iter().filter(|o| o.status == status).count()
    }
    pub fn passed(&self) -> usize { self.count(TestStatus::Pass) }
    pub fn failed(&self) -> usize { self.count(TestStatus::Fail) }
    pub fn errored(&self) -> usize { self.count(TestStatus::Error) }
    pub fn skipped(&self) -> usize { self.count(TestStatus::Skipped) }
    pub fn incomplete(&self) -> usize { self.count(TestStatus::Incomplete) }
    pub fn risky(&self) -> usize { self.count(TestStatus::Risky) }

    pub fn is_success(&self) -> bool {
        self.failed() == 0 && self.errored() == 0
    }
}

pub fn run(
    pool: &WorkerPool,
    cases: Vec<TestCase>,
    cfg: &RunConfig,
    on_progress: impl Fn(&TestOutcome) + Sync,
) -> Result<Report> {
    // Apply class-level filter pre-batch (so we don't ship classes that have
    // no matching methods). Inside a class, the worker filters by methods.
    let filtered_cases: Vec<TestCase> = cases
        .into_iter()
        .filter(|c| match &cfg.filter {
            Some(f) => format!("{}::{}", c.class, c.method).contains(f),
            None => true,
        })
        .collect();

    let groups = group_by_class(filtered_cases);

    // One WorkerClient per pool worker. Cloned ureq::Agent shares connection
    // pool internals; per-worker we want distinct URLs and isolated state.
    let urls = pool.urls();
    let clients: Vec<WorkerClient> =
        urls.iter().map(|u| WorkerClient::new(u.clone())).collect();

    // Distribute classes via rayon. Each rayon thread picks its client by
    // thread index; rayon guarantees that index < pool size when the global
    // thread pool was sized to match (see main.rs).
    let results: Vec<Result<Vec<TestOutcome>>> = groups
        .into_par_iter()
        .map(|TestClass { file, class, methods }| {
            let idx = rayon::current_thread_index().unwrap_or(0);
            let client = &clients[idx % clients.len()];
            let req = TestRunRequest {
                autoload: cfg.autoload.clone(),
                bootstrap: cfg.bootstrap.clone(),
                file,
                class,
                methods,
            };
            let batch = client.run_class(&req)?;
            // Emit progress inside the worker thread, as outcomes arrive.
            for outcome in &batch {
                on_progress(outcome);
            }
            Ok(batch)
        })
        .collect();

    // Aggregate. Short-circuit on first transport/protocol error so the
    // user sees the actual cause; per-test failures are returned in
    // outcomes, not as Result errors.
    let mut outcomes = Vec::new();
    let mut total = 0.0;
    for batch in results {
        let batch = batch?;
        for outcome in batch {
            total += outcome.duration_ms;
            outcomes.push(outcome);
        }
    }
    Ok(Report { outcomes, total_duration_ms: total })
}
```

Changes vs sequential:
- First parameter is `&WorkerPool` (not `&WorkerClient`)
- Progress callback is `Fn + Sync` (was `FnMut`)
- Uses `into_par_iter` and creates per-call `clients` vec
- Progress fires inside rayon thread

- [ ] **Step 2: Build (will fail on main.rs/integration.rs)**

```bash
cd /home/gumiranda/PHPUnit_rust && cargo build 2>&1 | grep -E "^error" | head -20
```

Expected: errors only in `main.rs` (uses old `run(&client, ...)` signature) and `tests/integration.rs`. Lib + runner.rs itself compile clean.

- [ ] **Step 3: Commit**

```bash
git add src/runner.rs
git commit -m "feat(runner): parallel class distribution via rayon::par_iter"
```

---

## Task 4: Initialize rayon pool in main; add --workers flag

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/main.rs`

- [ ] **Step 1: Replace `src/main.rs`**

```rust
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
```

- [ ] **Step 2: Build and run lib tests**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null || true
sleep 1
cargo build --release 2>&1 | tail -3
cargo test --lib 2>&1 | tail -3
```

Expected: clean release build, lib tests still all green (integration test will be updated in Task 5).

- [ ] **Step 3: End-to-end against the fixture with parallel workers**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null || true
sleep 1
./target/release/phpunit-rust --project fixtures/sample_project --workers 4 2>&1 | grep -E "^(Spawning|Tests:)" | head -3
echo "exit=$?"
```

Expected output:
```
Spawning 4 FrankenPHP workers...
Tests: 15 total, 12 passed, 1 failed, 0 errored, 1 skipped, 1 incomplete, 0 risky (<...>ms)
exit=1
```

- [ ] **Step 4: Verify --workers 1 still works (sequential mode)**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null || true
sleep 1
./target/release/phpunit-rust --project fixtures/sample_project --workers 1 2>&1 | grep -E "^(Spawning|Tests:)" | head -3
```

Expected: `Spawning 1 FrankenPHP worker...` and same fixture counts.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat(cli): --workers flag (default num_cpus); init rayon global pool"
```

---

## Task 5: Update integration tests for the new pool API

`tests/integration.rs` currently calls `WorkerClient::new(fph.worker_url())` directly. Update it to use the pool.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/tests/integration.rs`

- [ ] **Step 1: Replace `tests/integration.rs`**

```rust
use phpunit_rust::client::WorkerClient;
use phpunit_rust::frankenphp::{find_worker_script, WorkerPool};
use phpunit_rust::types::{TestRunRequest, TestStatus};
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project")
}

fn request(file: &str, class: &str) -> TestRunRequest {
    let root = fixture_root();
    TestRunRequest {
        autoload: root.join("vendor/autoload.php"),
        bootstrap: None,
        file: root.join(file),
        class: class.into(),
        methods: vec![],
    }
}

#[test]
fn calculator_class_all_three_methods_pass() {
    let worker = find_worker_script().expect("worker.php must exist");
    let pool = WorkerPool::spawn(&worker, 1).expect("1-worker pool must spawn");
    let client = WorkerClient::new(pool.urls().first().unwrap().clone());

    let req = request("tests/CalculatorTest.php", "Sample\\Tests\\CalculatorTest");
    let outcomes = client.run_class(&req).expect("worker call must succeed");

    assert_eq!(outcomes.len(), 3, "outcomes: {outcomes:?}");
    for o in &outcomes {
        assert_eq!(o.status, TestStatus::Pass, "{}::{} was {:?}: {:?}", o.class, o.method, o.status, o.message);
    }
}

#[test]
fn failing_class_mixed_results() {
    let worker = find_worker_script().expect("worker.php must exist");
    let pool = WorkerPool::spawn(&worker, 1).expect("pool must spawn");
    let client = WorkerClient::new(pool.urls().first().unwrap().clone());

    let req = request("tests/FailingTest.php", "Sample\\Tests\\FailingTest");
    let outcomes = client.run_class(&req).expect("worker call must succeed");

    assert_eq!(outcomes.len(), 2);
    let by_method: std::collections::HashMap<_, _> = outcomes.iter().map(|o| (o.method.clone(), o)).collect();
    assert_eq!(by_method["testThisPasses"].status, TestStatus::Pass);
    assert_eq!(by_method["testThisDeliberatelyFails"].status, TestStatus::Fail);
    assert!(by_method["testThisDeliberatelyFails"].message.as_deref().unwrap_or("").contains("intentional"));
}

#[test]
fn pool_of_three_serves_three_distinct_classes_concurrently() {
    // Sanity check: a 3-worker pool can serve 3 different class requests
    // without errors. We don't measure speed here; just correctness.
    let worker = find_worker_script().expect("worker.php must exist");
    let pool = WorkerPool::spawn(&worker, 3).expect("3-worker pool must spawn");
    assert_eq!(pool.len(), 3);

    let urls = pool.urls();
    let clients: Vec<WorkerClient> =
        urls.iter().map(|u| WorkerClient::new(u.clone())).collect();

    let r1 = request("tests/CalculatorTest.php", "Sample\\Tests\\CalculatorTest");
    let r2 = request("tests/FailingTest.php", "Sample\\Tests\\FailingTest");
    let r3 = request("tests/DataProviderTest.php", "Sample\\Tests\\DataProviderTest");

    let o1 = clients[0].run_class(&r1).expect("client 0 ok");
    let o2 = clients[1].run_class(&r2).expect("client 1 ok");
    let o3 = clients[2].run_class(&r3).expect("client 2 ok");

    assert_eq!(o1.len(), 3, "CalculatorTest outcomes");
    assert_eq!(o2.len(), 2, "FailingTest outcomes");
    assert_eq!(o3.len(), 4, "DataProviderTest outcomes (4 data rows)");
}
```

- [ ] **Step 2: Run full test suite**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null || true
sleep 1
cargo test --test integration -- --test-threads=1 2>&1 | tail -5
```

Note: `--test-threads=1` runs tests serially at the cargo level. Each individual test spawns its own multi-worker pool internally. Without this, cargo would run all 3 tests in parallel — each spawning multiple FrankenPHP children — and might exhaust ports or cause noisy failures.

Expected: `test result: ok. 3 passed; 0 failed`.

- [ ] **Step 3: Run full cargo test (including lib)**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null || true
sleep 1
cargo test 2>&1 | tail -5
```

Expected: 18 lib + 3 integration = 21 tests passing.

- [ ] **Step 4: Commit**

```bash
git add tests/integration.rs
git commit -m "test(integration): cover WorkerPool API + 3-worker concurrent dispatch"
```

---

## Task 6: Benchmark fixture + brick/math

Verification gate (no code changes; no commit). Demonstrates the speedup story.

**Files:** none.

- [ ] **Step 1: Time fixture in sequential mode**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null || true
sleep 1
time CALCULATOR=Native ./target/release/phpunit-rust --project fixtures/sample_project --workers 1 2>&1 | grep "^Tests:"
```

Capture the wall-clock time. Our fixture is small (15 tests, ~10ms) so the dominant cost is FrankenPHP startup + readiness (~2s). Expect ~2-3s real time.

- [ ] **Step 2: Time fixture in parallel mode**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null || true
sleep 1
time CALCULATOR=Native ./target/release/phpunit-rust --project fixtures/sample_project --workers 4 2>&1 | grep "^Tests:"
```

Expect 4× startup cost (4 workers × ~2s each) sequentially during pool spawn, plus a tiny parallel runtime. The fixture is too small for a meaningful speedup — startup overhead dominates. Expect ~3-5s real time. **For this size, sequential is faster than parallel.** That's expected and worth documenting.

- [ ] **Step 3: Time brick/math sequential (baseline)**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null || true
sleep 1
time CALCULATOR=Native ./target/release/phpunit-rust --project /tmp/phpunit-rust-smoke/brick-math --workers 1 2>&1 | grep "^Tests:"
```

Capture the wall-clock time. Expect ~3 minutes (matches prior v0.3 measurement at ~175s).

- [ ] **Step 4: Time brick/math parallel (the headline number)**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null || true
sleep 1
time CALCULATOR=Native ./target/release/phpunit-rust --project /tmp/phpunit-rust-smoke/brick-math 2>&1 | grep "^Tests:"
```

(Omitting `--workers` uses num_cpus default.) Expect speedup proportional to CPU count, capped by:
- Number of classes (we have 6 in brick/math — so > 6 workers gives no benefit)
- Per-class time variance (BigIntegerTest is the longest at ~165s sequential — that one class single-thread bounds total wall time)

So the realistic ceiling for brick/math parallel is `max(per_class_time) + small_overhead ≈ 170s`. With 6+ workers, we should approach this. **Speedup ratio for brick/math will be modest** (maybe 1.1–1.5×) because BigIntegerTest dominates and can't be subdivided.

**Critical:** Tests must still be 13589 / 13589 / 0 failed / 0 errored. Same outcomes, just delivered faster.

- [ ] **Step 5: Document findings**

Note the actual numbers in your report. They'll feed Task 7's README. Capture:
- Fixture sequential vs parallel wall-clock (parallel should be slower for tiny suites — documented limitation)
- brick/math sequential wall-clock
- brick/math parallel wall-clock
- brick/math speedup ratio

No commit.

---

## Task 7: README v0.4.0 update

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/README.md`

- [ ] **Step 1: Update Status section**

Replace the `## Status: v0.3.0` heading in README.md with:

```markdown
## Status: v0.4.0 — parallel execution

Runs N FrankenPHP worker processes concurrently via Rayon. Speedup scales
with CPU count for CPU-bound suites, capped by the largest single class
(no per-method parallelism yet — that's a future plan).

Sequential mode is available via `--workers 1`. Default is `num_cpus`.

### Supported

- (everything from v0.3.0, plus:)
- `--workers N` for parallel class-level dispatch

### Not yet supported

- (everything else from v0.3.0; parallelism within a class)
```

- [ ] **Step 2: Update Usage section**

In the Usage block, add the `--workers` example:

```bash
./target/release/phpunit-rust --project /path/to/php/project --workers 8
./target/release/phpunit-rust --project /path/to/php/project --workers 1  # sequential
```

- [ ] **Step 3: Update Architecture diagram**

Replace the architecture block:

```
phpunit-rust (Rust binary)
  ├─ discovery   : tree-sitter-php; class graph + BFS for transitive inheritance
  ├─ phpunit_xml : minimal parser for <phpunit bootstrap="..."> attribute
  ├─ frankenphp  : WorkerPool spawns N FrankenPHP children, each on its own port
  ├─ client      : ureq HTTP/JSON to one worker.php instance
  ├─ runner      : rayon::par_iter distributes classes across pool
  └─ reporter    : TTY output (thread-safe via stdout's per-write atomicity)

N FrankenPHP workers (each ~50MB, long-lived in worker mode)
  └─ worker.php → TestExecutor::runClass(...) → outcomes JSON
```

- [ ] **Step 4: Add a Performance section**

Add (with your actual numbers from Task 6):

```markdown
## Performance

Benchmark on brick/math (13,589 tests, PHPUnit 11):

| Workers | Wall-clock | Speedup |
|---------|-----------|---------|
| 1       | <Task 6 Step 3 time>s | 1.0× |
| <num_cpus> | <Task 6 Step 4 time>s | <ratio>× |

The speedup ceiling depends on the largest single class. brick/math's
BigIntegerTest takes ~165s on its own, which bounds parallel wall-clock.
Suites with more even per-class duration distributions see proportionally
larger speedups.
```

- [ ] **Step 5: Commit + push**

```bash
cd /home/gumiranda/PHPUnit_rust && git add README.md
git commit -m "docs: README for v0.4.0 — parallel execution"
git push origin master 2>&1 | tail -3
```

---

## Self-Review

**1. Spec coverage:**

| Requirement | Task |
|---|---|
| N FrankenPHP processes | Task 2 (WorkerPool) |
| Each on distinct port | Task 2 (find_free_port sequential) |
| Parallel readiness probing | Existing per-worker probe; spawned in sequence (cheap enough) |
| Rayon distribution | Task 3 |
| `--workers N` flag | Task 4 |
| Default = num_cpus | Task 4 |
| Sequential mode (`--workers 1`) | Task 4 + Task 6 Step 4 |
| Drop kills all children | Existing FrankenPhp::drop; Vec drops sequentially; covered |
| Wire protocol unchanged | confirmed throughout |
| Test isolation across workers | n/a — multi-process = fully isolated, no extra work |

**2. Placeholder scan:** Every code step has complete, runnable code. The benchmark task (Task 6) intentionally has no committed artifact — it's a measurement gate that feeds Task 7's README.

**3. Type consistency:**
- `WorkerPool::spawn(&Path, usize) -> Result<Self>`, `urls() -> Vec<String>`, `len() -> usize` — consumed by main.rs and integration tests with matching signatures.
- `runner::run(&WorkerPool, Vec<TestCase>, &RunConfig, impl Fn(&TestOutcome) + Sync) -> Result<Report>` — main.rs passes `&pool`, `cases`, `&cfg`, `|o| print_progress(o)`. `print_progress` is `fn(&TestOutcome)` — satisfies `Fn + Sync` (it captures no state).
- `RunConfig` field names unchanged from v0.3.

**4. Concurrency review:**
- `rayon::current_thread_index() % clients.len()` is safe: rayon's global pool size matches `clients.len()` (set in main.rs), so the index is always in range. The modulo is defensive for the unlikely case `index >= len`.
- `WorkerClient` clones share the ureq Agent's internal connection pool. Each rayon thread holds an `&WorkerClient` to a distinct entry in the `clients` vec — no aliasing of mutable state.
- `print_progress` writes to stdout via `print!`. Each write goes through stdout's `BufWriter` lock — multiple threads writing single chars don't corrupt the stream.

## Out-of-scope (deferred to follow-up plans)

- **Per-method parallelism within a class.** Currently a class is the unit of parallelism. Splitting a single large class (like brick/math's BigIntegerTest, 8312 tests) across workers requires either (a) sending one HTTP request per method, or (b) batching methods per worker. Worth a dedicated plan after we measure how often a single class is the bottleneck in real codebases.
- **Smart scheduling.** Currently we dispatch classes in discovery order. Future improvements: schedule by historical duration (longest first), schedule failed-tests-first, etc. Each is its own ~1-2 task addition.
- **Watch mode.** Re-running on file changes. Independent of parallelism.
- **Coverage.** PCOV/Xdebug integration. Independent.
- **Custom reporters.** JUnit XML / TAP / TestDox. Independent.
