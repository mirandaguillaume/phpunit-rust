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
