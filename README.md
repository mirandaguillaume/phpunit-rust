# phpunit-rust

A Rust orchestrator that runs PHPUnit tests in parallel using forked PHP
workers — no FrankenPHP, no HTTP, no daemon. One PHP master loads the
project's autoloader and bootstrap once; N children are forked via
`pcntl_fork()` so they inherit the warmed-up interpreter via copy-on-write.
Tests delegate to the project's own PHPUnit installation.

## Status: v0.7.0 — exact test-count parity on 4 of 5 benched OSS suites

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

**Test discovery** (tree-sitter-based, our `discovery` crate):

- `testXxx` naming, `/** @test */` PHPDoc, `#[Test]` attribute (including
  stacked with `#[Group(...)]` and other decorations)
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
- `<groups><exclude>`
- `<listeners>` parsed but **not dispatched** (see "Not yet supported")

**CLI flags:**

- `--project`, `--bootstrap`, `--filter`, `--workers`, `--configuration`
- `--coverage-format clover|json --coverage-out path` (build with
  `--features coverage`)

**Robustness:**

- SIGINT / SIGKILL on `phpunit-rust` reliably kills the PHP master and
  every forked child via kernel `PR_SET_PDEATHSIG` + PHP signal handlers
  — no orphan workers, no zombie 100%-CPU PHP processes after a Ctrl-C
- `setUpBeforeClass` and `tearDownAfterClass` failures emit per-test
  error outcomes instead of swallowing every test in the class

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
  │   ├─ phpunit_xml    bootstrap, <testsuites>, <php><const>,
  │   │                 <groups><exclude>, <listeners>
  │   ├─ provider_enum  pre-fork PHP pass to count provider rows
  │   ├─ fork_pool      pipe-managed N-slot fork pool (CLOEXEC, PDEATHSIG)
  │   ├─ runner         work-stealing queue, LPT scheduling, row split
  │   └─ reporter       TTY progress + summary (mpsc-driven)
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

Benchmarked on Linux/PHP 8.1.33 with 4 workers against real OSS suites.
Median of 3 runs. "vanilla" is `./vendor/bin/phpunit` (one process).

| Project | vanilla | phpunit-rust (4w) | Speedup | Tests |
|---|---:|---:|---:|---:|
| carbon | 20.9s | 8.5s | **2.45×** | 6169 |
| doctrine-orm | 1.60s | 1.60s | tied | 3478 |
| faker | 1.00s | 0.85s | **1.17×** | 1402 / 1416 |
| php-parser | 0.40s | 0.35s | 1.13× | 1887 |
| guzzle-psr7 | 0.14s | 0.20s | — | 1088 |
| brick-math (Docker, PHP 8.4) | 183s | 167s | 1.09× | 13589 |

CPU-bound suites with many independent classes (carbon especially) see
the biggest gains. Sub-second suites are dominated by PHP startup cost
and the fork-pool overhead doesn't amortize; for those, prefer
`--workers 1` or run vanilla.

For a project requiring PHP 8.4 (e.g. PHPUnit's own test suite,
brick-math), a Docker harness in `bench/Dockerfile.php84` builds a
container with `pcntl` pre-compiled.

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
