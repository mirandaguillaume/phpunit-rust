# phpunit-rust

A Rust orchestrator that runs PHPUnit tests in parallel using forked PHP
workers — no FrankenPHP, no HTTP, no daemon. One PHP master loads the
project's autoloader and bootstrap once; N children are forked via
`pcntl_fork()` so they inherit the warmed-up interpreter via copy-on-write.
Tests delegate to the project's own PHPUnit installation.

## Status: v0.8.0 — exact test-count parity on PHPUnit's own suite + 5 OSS projects

The runner spawns one PHP master, forks N children, then streams test
classes (and individual data-provider rows for heavy providers) over
per-child pipes using a work-stealing queue. LPT (longest-processing-
time-first) scheduling means the heaviest classes start on all workers
concurrently instead of stranding one at the end.

**Test-count parity** is the goal: for every project we benchmark, the
`Tests: N` line we report should match what `./vendor/bin/phpunit` reports.
Today's scoreboard:

| Project | vanilla | phpunit-rust | Status |
|---|---:|---:|:---|
| phpunit (own suite) | 5029 | **5029** | EXACT ✓ |
| carbon | 6169 | **6169** | EXACT ✓ |
| doctrine-orm | 3478 | **3478** | EXACT ✓ |
| php-parser | 1887 | **1887** | EXACT ✓ |
| guzzle-psr7 | 1088 | **1088** | EXACT ✓ |
| faker | 1416 | 1402 | −14 *(Symfony PhpUnitTestsListener emits 14 synthetic SkippedTestCase wrappers we don't replicate — would require user-listener dispatch)* |
| brick-math (PHP 8.4, Docker) | 13589 | **13589** | EXACT ✓ |

Behavioural breakdowns may still differ on a small handful of tests
(e.g. doctrine-orm: ~9 tests we pass that vanilla errors on; guzzle-psr7:
one test that passes alone but fails after another due to state pollution
in our worker process) — these are not count-parity issues.

### Supported

**TestCase API** (delegated to the project's PHPUnit, so all of it works):

- All `TestCase` assertions, mocks (`createMock`, `MockBuilder`), expectation
  chaining
- `expectException`, `expectExceptionMessage`, `expectExceptionCode`
- `markTestSkipped`, `markTestIncomplete`
- `setUp` / `tearDown`, `setUpBeforeClass` / `tearDownAfterClass`
- PHPUnit 10 attribute-style lifecycle hooks: `#[Before]`, `#[After]`,
  `#[BeforeClass]`, `#[AfterClass]`

**Test discovery** (tree-sitter-based, our `discovery` crate):

- `testXxx` naming, `/** @test */` PHPDoc, `#[Test]` attribute (including
  stacked with `#[Group(...)]` and other decorations)
- `#[Ticket('...')]` attribute (parsed; used by some legacy suites for metadata)
- Inheritance chains: fully-qualified extends, abstract base classes
  outside test dirs (resolved via `composer.json` autoload)
- Custom-framework base classes: any FQCN whose last segment ends in
  `TestCase` is recognised (catches `PHPStanTestCase`, Symfony's
  `KernelTestCase` / `WebTestCase`, etc.)
- Case-insensitive method-name dedup along the inheritance chain (PHP
  semantics: a subclass `testfoo` overrides a parent `testFoo`)

**Data providers — every form PHPUnit supports:**

- `#[DataProvider("methodName")]` attribute
- `@dataProvider methodName` PHPDoc
- `#[TestWith([1, 2])]` repeatable attribute
- `#[TestWithJson('[1, 2]')]` repeatable attribute
- `@testWith [1, 2]\n        [3, 4]` PHPDoc block

Row counts are enumerated by a one-shot pre-fork PHP pass; heavy providers
(≥ 15 rows) get split across workers via stride filtering
(`row_i % N == chunk_index`).

**Skip / requires — all PHPUnit 10 attribute + PHPDoc forms:**

- `#[RequiresPhp]`, `#[RequiresPhpExtension]`, `#[RequiresFunction]`,
  `#[RequiresMethod]`, `#[RequiresOperatingSystem]`,
  `#[RequiresOperatingSystemFamily]`, `#[RequiresSetting]`,
  `#[RequiresPhpunit]`
- `@requires` PHPDoc equivalents
- All checked **before** `setUpBeforeClass` so version-gated entity files
  don't crash workers with uncatchable `E_COMPILE_ERROR`

**Groups:**

- `#[Group('name')]` and `/** @group name */` (class- and method-level)
- Inherited from the parent class along the test chain
- `phpunit.xml` `<groups><exclude><group>name</group>` filters them out

**Test dependencies:**

- `#[Depends('method')]` and `@depends method`
- Topological sort + return-value injection within a class

**phpunit.xml:**

- `bootstrap` attribute
- `<testsuites>` (multiple suites, **per-suite** `<exclude>`: a directory
  excluded by suite A but explicitly included by suite B is still walked
  via B)
- `<php><const>` declarations
- `<php><env>` and `<php><server>` (sets `$_ENV`/`$_SERVER`; `force`
  attribute honoured for `<env>`)
- `<php><ini>` (applied before autoload/bootstrap via `ini_set()`)
- `<groups><exclude>`
- `<listeners>` parsed but **not dispatched** (see "Not yet supported")

**CLI flags:**

- `--project`, `--bootstrap`, `--filter`, `--workers`, `--configuration`
- `--group <name>`, `--exclude-group <name>` (filter by `#[Group]` / `@group`)
- `--testsuite <name>` (run a named suite from `phpunit.xml`)
- `--stop-on-failure` (halt after the first failing test)
- `--list-tests` (print `Class::method` lines then exit, no tests run)
- `--bake-mocks` (rewrite `createMock()` calls to anonymous-class stubs
  before execution; requires PSR-4 resolvable interfaces)
- `--coverage-format clover|json --coverage-out path` (build with
  `--features coverage`)

**Robustness:**

- SIGINT / SIGKILL on `phpunit-rust` reliably kills the PHP master and
  every forked child via kernel `PR_SET_PDEATHSIG` + PHP signal handlers
  — no orphan workers, no zombie 100%-CPU PHP processes after a Ctrl-C
- Each forked child becomes its own process-group leader (`posix_setpgid`);
  shutdown sends `SIGKILL` to the entire process group so grandchildren
  spawned by a test (via `proc_open`, `shell_exec`, etc.) are also reaped
- `setUpBeforeClass` and `tearDownAfterClass` failures emit per-test
  error outcomes instead of swallowing every test in the class
- Cross-class data-provider dependencies resolved via a secondary
  autoloader (Rust writes the FQCN → file index, PHP registers it with
  `spl_autoload_register`); provider exceptions are isolated per-method
  rather than crashing the whole class

**Static coverage** via the sibling `analyzer` crate (mago AST + per-test
attribution; no Xdebug / PCOV needed).

### Not yet supported (deferred)

- Generic `<listeners>` dispatch (we parse the entries but don't execute
  user listener code — affects projects using Symfony's PhpUnitTestsListener
  for `@group legacy` handling)
- `<extensions>` (PHPUnit 10+ extension API)
- `@runInSeparateProcess` / `#[RunInSeparateProcess]`
- Runtime coverage (PCOV/Xdebug) — static analysis only for now
- JUnit XML / TAP / TestDox reporters
- Watch mode
- Risky test detection (no assertions, unexpected output, etc.)

## Requirements

- Rust 1.75+
- PHP 8.1+ with the `pcntl` extension on `$PATH` (Linux/macOS;
  Windows not supported — `pcntl_fork()` is POSIX-only)
- Project under test: `composer install`'d, PHPUnit 10 or 11 on its
  vendor path. Tested against PHPUnit 10.5 and 11.5.

## Usage

```bash
cargo build --release

# auto-detect phpunit.xml, use 4 workers
./target/release/phpunit-rust --project /path/to/php/project

# explicit worker count (default 4)
./target/release/phpunit-rust --project /path/to/php/project --workers 8

# sequential (no parallelism overhead — best for tiny suites)
./target/release/phpunit-rust --project /path/to/php/project --workers 1

# filter by class or method name
./target/release/phpunit-rust --project /path/to/php/project --filter MyClass

# explicit bootstrap
./target/release/phpunit-rust --project /path/to/php/project --bootstrap tests/bootstrap.php

# static coverage (requires --features coverage at build time)
./target/release/phpunit-rust --project /path/to/php/project \
    --coverage-format clover --coverage-out coverage.xml
```

`phpunit.xml` / `phpunit.xml.dist` is auto-detected at the project root.
`--configuration` overrides the search path.

## Architecture

```
Workspace (Cargo)
  ├─ crates/discovery   PHP test discovery (tree-sitter-php)
  │                     · class graph + transitive-inheritance BFS
  │                     · #[Test], @test, #[DataProvider], @dataProvider,
  │                       #[TestWith], @testWith, #[Group], @group
  │                     · custom-framework TestCase bases
  ├─ crates/runner      phpunit-rust binary
  │   ├─ phpunit_xml    bootstrap, <testsuites>, <php><const/env/server/ini>,
  │   │                 <groups><exclude>, <listeners>
  │   ├─ provider_enum  pre-fork PHP pass to count provider rows
  │   ├─ fork_pool      pipe-managed N-slot fork pool (CLOEXEC, PDEATHSIG,
  │   │                 process-group kill, class-map temp file)
  │   ├─ runner         work-stealing queue, LPT bin-packing, row split
  │   ├─ mock_bake      PSR-4 resolver + --bake-mocks preprocessing
  │   └─ reporter       TTY progress + summary (mpsc-driven)
  ├─ crates/mock_baker  tree-sitter createMock() → anonymous-class rewriter
  └─ crates/analyzer    static PHP coverage via mago AST
                        · per-test attribution
                        · Clover / JSON output (--features coverage)

PHP master (php/worker_fork.php)
  ├─ Load autoload + bootstrap + project constants ONCE
  ├─ Install SIGTERM/SIGINT/SIGHUP handlers → kill children → exit
  └─ pcntl_fork() × N → children inherit the warmed interpreter via COW

PHP child (one of N)
  ├─ Read newline-delimited BatchPlan JSONs on its stdin pipe
  ├─ For each plan: require_once test file, TestExecutor::runClass(...)
  ├─ Stream TestOutcome JSON lines on its stdout pipe
  ├─ Emit {"batch_done": true} between plans (work-stealing ready signal)
  └─ Exit cleanly on EOF (Rust closed our stdin)
```

The Rust master holds a `VecDeque<BatchPlan>` and one reader thread per
child. Each reader forwards `(slot, TestOutcome | BatchDone | Eof)` over
an `mpsc` channel to the main dispatcher loop, which sends the next plan
to whichever child reported `BatchDone` first. When the queue empties,
idle slots get their stdin pipes closed and the children exit on EOF.

Heavy data-provider methods (≥ 15 enumerated rows) are split into up to 4
stride-partitioned plans, each running on a different worker via the
existing `RowFilter` (`chunk_index % total_chunks`) inside `TestExecutor`.
Plain methods stay in a single class-level plan (splitting them would
multiply the `setUpBeforeClass` cost without paying for itself).

## Performance

Benchmarked on Linux/PHP 8.1.33 against real OSS suites. Median of 3
runs each. "vanilla" is `./vendor/bin/phpunit` (one process); `1w` /
`2w` / `4w` / `8w` are our fork pool at that worker count.

### Reference run — May 2026

End-to-end bench of seven real OSS suites on the same Linux laptop,
8 workers (`-w 8 -k 20`), Docker projects mounted with `--tmpfs /tmp`
and the host's `/tmp` already on tmpfs. Δ columns use a uniform
sign convention: **`+` = phpunit-rust wins, `−` = vanilla wins**.

| Project (tests) | Vanilla wall | Rust wall | Speedup | Wall saved | Vanilla RAM | Rust RAM | RAM saved | user-CPU vanilla | user-CPU rust | CPU overhead |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| guzzle-psr7 (1227) | 0.23 s | 0.39 s | 0.59× | **−0.16 s** | 50 MB | 45 MB | +10 % | 0.19 s | 1.43 s | +750 % |
| nikic/php-parser (1878) | 0.55 s | 0.65 s | 0.85× | **−0.10 s** | 68 MB | 64 MB | +6 % | 0.48 s | 2.55 s | +430 % |
| fakerphp/faker (1402) | 1.12 s | 0.94 s | 1.19× | **+0.18 s** | 337 MB | 113 MB | +67 % | 0.55 s | 5.18 s | +772 % |
| Carbon (6139) | 23.25 s | 8.15 s | 2.85× | **+15.10 s** | 200 MB | 158 MB | +21 % | 21.65 s | 47.40 s | +149 % |
| monolog (1162, PHP 8.3) | 4.30 s | 1.41 s | 3.05× | **+2.89 s** | 59 MB | 33 MB | +44 % | 0.41 s | 2.19 s | +438 % |
| rector (5207, PHP 8.3) | 19.68 s | 5.78 s | 3.40× | **+13.90 s** | 661 MB | 176 MB | **+73 %** | 17.65 s | 37.85 s | +103 % |
| phpstan-src (11928, PHP 8.3) | 96.22 s | 23.59 s | **4.08×** | **+72.63 s** | 1749 MB | 295 MB | **+83 %** | 93.17 s | 119.19 s | +48 % |

`+ %` on RAM = peak RSS reduction by phpunit-rust. CPU overhead = extra
user-CPU seconds we burn for the parallelism; the worst case (guzzle's
`+750 %`) costs **1.2 vCPU-seconds of compute** to save nothing — it
loses 0.16 s of wall — but on rector the same overhead bracket
(`+103 %`) buys back **13.9 s of wall and 485 MB of resident set**.

#### Reading the trade-off in actual money

Indexed against a fully-loaded senior PHP engineer salary in the US
mid-2026 (~$90/hr — base ~$65-70/hr W2 + ~30 % employer load),
AWS m7i.large on-demand at $0.1008/hr, and GitHub Actions Linux
small at $0.006/min (post January-2026 reduction), the wall-time
savings dominate every other cost line by ~3 orders of magnitude.
Worked example: a 10-engineer team running rector's suite 30 times
each weekday.

| Cost line | Vanilla | Rust | Annual savings |
|---|---:|---:|---:|
| Engineer wait time @ $90/hr | $10.6 k / yr | $3.1 k / yr | **~$7.5 k** per engineer × 10 = **~$22.9 k** |
| Cloud compute @ $0.1008/hr | ~$31 / yr | ~$9 / yr | ~$22 |
| GitHub Actions runner @ $0.006/min | ~$210 / yr | ~$61 / yr | ~$149 |

The CPU we pay for is essentially free; the wall we save is paid back
in engineer-hours.

Salary anchor: [Senior PHP Developer 2026 — Salary.com](https://www.salary.com/research/salary/hiring/senior-php-developer-salary)
($70/hr W2). EC2 reference: [m7i.large — Vantage Instances](https://instances.vantage.sh/aws/ec2/m7i.large)
(updated 2026-05-27). CI rate: [GitHub Actions 2026 pricing](https://resources.github.com/actions/2026-pricing-changes-for-github-actions/).

#### Known limitation — process-isolation hangs (PHPUnit-itself)

The PHPUnit project's own test suite was attempted as an eighth
data point (PHP 8.4, Docker, `bench/Dockerfile.php84` + `--tmpfs /tmp`).
Vanilla median: **133.66 s** (3 runs: 132.31, 133.66, 141.10 s), ~108 MB
RSS. phpunit-rust did **not** complete: a worker hangs partway
through, our reader thread blocks on its (still-open) stdout pipe,
no `SIGCHLD` ever fires because the child is alive — just stuck.

The root cause is PHPUnit's own `@runInSeparateProcess` / end-to-end
fixtures: a test spawns a sub-PHP process via `proc_open`, and if
that sub-process never returns (e.g. waiting on input the worker
never produces), the parent worker stays in `read()` indefinitely.
The lost-batch recovery added in `feat(runner): recover lost
batches when a worker dies mid-run` only triggers on actual death
(non-zero exit / signal), not on a stuck-but-alive worker.

Fix is straightforward but unwritten: a per-slot inactivity
watchdog. If a slot has an in-flight batch and hasn't emitted any
outcome for N seconds, the dispatcher SIGKILLs the worker, treats
the batch as crashed (lost-batch path takes over), and the master's
SIGCHLD handler respawns a clean child. Tracked as a follow-up.

### Worker scaling

| Project | vanilla | 1w | 2w | 4w | 8w | Best speedup vs vanilla |
|---|---:|---:|---:|---:|---:|---:|
| carbon (6169 tests) | 21.4s | 31.7s | 15.4s | 8.7s | **5.8s** | **3.7×** at 8w |
| doctrine-orm (3478 tests) | 1.62s | 2.15s | 1.74s | 1.66s | **1.59s** | 1.02× at 8w (≈ tied) |
| faker (1402 tests) | 1.08s | 1.22s | **0.81s** | 0.81s | 0.82s | 1.34× at 2w |
| php-parser (1887 tests) | 0.38s | 0.44s | 0.36s | **0.34s** | 0.38s | 1.13× at 4w |
| guzzle-psr7 (1088 tests) | 0.14s | 0.21s | 0.20s | 0.19s | 0.20s | — (vanilla wins) |

What this says:

- **CPU-bound suites with many independent classes** (carbon) scale
  cleanly: 1→8 workers gives 5.4× speedup (68 % parallel efficiency),
  and 8 workers buys 3.7× over vanilla's single-process run.
- **Mixed suites** (faker, php-parser) peak at 2–4 workers and degrade
  past that: per-class fork/dispatch overhead starts to dominate when
  tests are short.
- **Suites of fast-erroring tests** (doctrine-orm functional tests bail
  out in setUp because no DB is configured) are essentially tied with
  vanilla — the parallelism can't help when tests take <1 ms each and
  there's no real work to spread.
- **Sub-second suites** (guzzle-psr7) can't beat vanilla at any worker
  count: our fork-pool startup is ~50 ms, vanilla starts in ~10 ms.
  For these, run vanilla.

The rule of thumb: **use `--workers N` where N is between 2 and the
number of physical cores you have, capped at half the test class
count.** Default is 4. If a 1-second suite slows down at 4 workers,
drop to 1 — the parallelism overhead isn't free.

### Docker (PHP 8.4 projects)

Some OSS suites require a newer PHP than the host. Build the Docker
image once (`docker build -f bench/Dockerfile.php84 -t phpunit-rust-bench:php84 .`)
and the `bench/bench_docker.sh` wrapper handles `composer install`
and the bind-mount of our release binary + PHP scripts.

| Project | vanilla | phpunit-rust (4w) | Speedup | Tests |
|---|---:|---:|---:|---:|
| brick-math | 183s | 167s | 1.09× | 13589 |

(More Docker projects pending; brick-math is the heavyweight reference
point — 13 k tests across 6 classes, almost entirely CPU-bound arithmetic.)

#### Docker (PHP 8.3, defaults)

Measured single-run on the same Linux host using `bench/Dockerfile.php83`
with `--tmpfs /tmp` + `--worker-memory-limit 4G`, 8 workers, K=20 recycling.
Wall and RSS captured by `/usr/bin/time` inside the container.

| Project (tests) | vanilla wall | rust wall | speedup | vanilla RSS | rust RSS |
|---|---:|---:|---:|---:|---:|
| monolog (1162) | 4.28 s | **1.38 s** | **3.10×** | 60 MB | **35 MB** |
| rector (5207) | 19.63 s | **4.00 s** | **4.91×** | 676 MB | **157 MB** |
| phpstan-src (12397) | ≈ 97 s ¹ | **22.92 s** | **≈ 4.24×** | 1.83 GB ¹ | **313 MB** |

¹ phpstan-src needs `php -d memory_limit=2G` on vanilla — its tests
allocate the analyzer in-memory per case. Default 128 MB makes vanilla
crash within 3 s.

Rector + phpstan exercise three of the runner's bug-class repairs at
once: per-class state isolation (stream wrappers, error handlers),
in-flight batch recovery when a child fatal kills a worker mid-run,
and skipping opcache pre-warm for the fixture-style files those
suites pack into the test roots.

#### Running inside Docker or CI

Two flags matter when running the runner inside a container:

- `--tmpfs /tmp:rw,exec,nosuid,size=4g` — analyzer-style suites (phpstan,
  psalm, rector) write multi-GB of disposable scratch under `/tmp` per
  test. With the container's default overlay `/tmp` these writes hit the
  storage driver — wasted IO bandwidth and disk wear. tmpfs keeps them
  in RAM. Measured on phpstan-src (12 k tests): 1.83 GB → 11 MB filesystem
  writes, wall unchanged. The host's `/tmp` is already tmpfs on modern
  Linux, so this only matters in containers.

- `--init` (or `tini`) — without it, Ctrl-C on `docker run` orphans the
  PHP fork children; the daemon never signals the container's main
  process, and the workers spin until the OOM killer notices. Both
  bundled wrappers (`bench/bench_docker.sh` and
  `scripts/docker-test-bake.sh`) set both flags automatically; pass them
  yourself when invoking phpunit-rust directly.

Memory-heavy suites (phpstan-src needs ≥ 2 GB resident, rector ~700 MB)
need `--worker-memory-limit 4G` to keep the fork children from being
killed by their own self-imposed cap. The master process always runs
with `memory_limit=-1` because it pre-warms opcache for every test
file before forking.

#### Environment variables

Three opt-in knobs surface from the PHP master, useful for debugging
and A/B benchmarking:

- `PHPUNIT_RUST_TIMING=1` — log master phase timings to stderr
  (autoload, bootstrap, opcache pre-warm, fork) as `[TIMING]` lines.
- `PHPUNIT_RUST_OPCACHE_THRESHOLD=N` — override the default
  50-test-file threshold for pre-warming opcache. Pass `99999` to
  disable pre-warm entirely (useful when investigating master crashes
  on very large suites).
- `PHPUNIT_RUST_NO_ISOLATION=1` — disable the per-batch fresh-fork
  applied to stateful test classes (those calling
  `stream_wrapper_register`, `set_error_handler`, …). Costs ~14 % on
  state-sensitive suites but exposes their pollution for diagnosis.

## Benchmarking

```bash
# Host bench (uses your local PHP). Defaults to 3 runs, 4 workers.
bench/bench_host.sh                              # all OSS projects
bench/bench_host.sh carbon doctrine-orm          # subset
RUNS=5 WORKERS=8 bench/bench_host.sh             # tuning

# Docker bench for PHP-version-specific projects.
docker build -f bench/Dockerfile.php84 -t phpunit-rust-bench:php84 .
bench/bench_docker.sh phpunit-itself
```

The host script expects projects under `/tmp/phpunit-rust-smoke/<name>`
with composer install run; the Docker script handles composer install
inside the container on first use.
