# phpunit-rust

A Rust orchestrator that runs PHPUnit tests in parallel using forked PHP
workers — no FrankenPHP, no HTTP, no daemon. One PHP master loads the
project's autoloader and bootstrap once; N children are forked via
`pcntl_fork()` so they inherit the warmed-up interpreter via copy-on-write.
Tests delegate to the project's own PHPUnit installation.

## Status: v0.7.0 — fork-based pool + work-stealing + per-row dispatch

The runner spawns one PHP master, forks N children, then streams test
classes (and individual data-provider rows for heavy providers) over
per-child pipes using a work-stealing queue. The class scheduler uses LPT
(longest-processing-time-first) ordering so the heaviest classes start on
all workers concurrently rather than stranding one at the end.

### Supported

- All `TestCase` assertions — worker delegates to the project's PHPUnit
- Mocks (`createMock`, `MockBuilder`, expectation chaining)
- `expectException`, `expectExceptionMessage`, `expectExceptionCode`
- Data providers (`#[DataProvider]` / `@dataProvider`) — runtime row
  enumeration via a one-shot pre-fork PHP pass, heavy providers split
  across workers via stride filtering (`row_i % N == chunk_index`)
- Test dependencies (`#[Depends]`, return-value passing within a class)
- `markTestSkipped`, `markTestIncomplete`
- `setUp` / `tearDown`, `setUpBeforeClass` / `tearDownAfterClass`
- `#[RequiresPhp]`, `#[RequiresPhpExtension]`, `#[RequiresFunction]`,
  `#[RequiresMethod]`, `#[RequiresOperatingSystem]`,
  `#[RequiresOperatingSystemFamily]` (checked **before**
  `setUpBeforeClass` so version-gated entity files don't crash workers
  with uncatchable `E_COMPILE_ERROR`)
- `@requires` PHPDoc annotations (PHP version, extensions, functions, OS)
- `phpunit.xml` — `bootstrap`, `<testsuites>` directories/excludes,
  `<php><const>`
- `--bootstrap`, `--filter`, `--workers`, `--configuration` CLI flags
- Test discovery: `testXxx` naming, `@test` annotation, `#[Test]` attribute
- Inheritance chains: fully-qualified extends + abstract base classes
  outside test dirs resolved via `composer.json` autoload
- Robust shutdown: SIGINT/SIGKILL on phpunit-rust reliably kills the PHP
  master and every forked child (kernel `PR_SET_PDEATHSIG` + PHP signal
  handlers — no orphan workers)
- Optional static coverage via the sibling `analyzer` crate (build with
  `--features coverage`, then `--coverage-format clover|json --coverage-out path`)

### Not yet supported (deferred)

- `@runInSeparateProcess`
- Runtime coverage (PCOV/Xdebug) — static analysis only for now
- JUnit XML / TAP / TestDox reporters
- Watch mode
- Custom PHPUnit extensions/listeners
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
`--configuration` overrides the search path. The runner reads `bootstrap`,
`<testsuites>` directories/excludes, and `<php><const>` entries.

## Architecture

```
phpunit-rust (Rust binary, crates/runner)
  ├─ discovery       : tree-sitter-php; class graph + transitive-inheritance BFS
  │                    + #[DataProvider]/@dataProvider extraction per method
  ├─ phpunit_xml     : parser for bootstrap, <testsuites>, <php><const>
  ├─ provider_enum   : pre-fork PHP pass — enumerate data-provider row counts
  ├─ fork_pool       : pipe-managed N-slot PHP fork pool with CLOEXEC hygiene
  ├─ runner          : work-stealing queue + LPT scheduling + per-row split
  └─ reporter        : TTY progress + summary (mpsc-driven, thread-safe)

PHP master (php/worker_fork.php)
  ├─ Load autoload + bootstrap + project constants ONCE
  ├─ Install SIGTERM/SIGINT/SIGHUP handlers (kill children → exit)
  └─ pcntl_fork() × N → children inherit the warmed interpreter via COW

PHP child (one of N)
  ├─ Read newline-delimited BatchPlan JSONs on its stdin pipe
  ├─ For each plan: require_once test file, TestExecutor::runClass(...)
  ├─ Stream TestOutcome JSON lines on its stdout pipe
  ├─ Emit {"batch_done": true} between plans (work-stealing ready signal)
  └─ Exit cleanly on EOF (Rust closed our stdin)
```

The Rust master holds a `VecDeque<BatchPlan>` and one reader thread per
child. Each reader forwards `(slot, TestOutcome | BatchDone | Eof)` over an
`mpsc` channel to the main dispatcher loop, which sends the next plan to
whichever child reported `BatchDone` first. When the queue empties, idle
slots get their stdin pipes closed and the children exit on EOF.

Heavy data-provider methods (≥ 15 enumerated rows) are split into up to 4
stride-partitioned plans, each running on a different worker via the
existing `RowFilter` (`chunk_index % total_chunks`) inside `TestExecutor`.
Plain methods stay in a single class-level plan (splitting them would
multiply the `setUpBeforeClass` cost without paying for itself).

The sibling `crates/analyzer` provides static PHP coverage (mago-based AST
walks, per-test attribution) and is consumed by the runner behind the
optional `coverage` feature.

## Performance

Benchmarked on Linux/PHP 8.1.33 with 4 workers against real OSS suites.
Median of 3 runs. "vanilla" is `./vendor/bin/phpunit` (one process).

| Project | vanilla | phpunit-rust (4w) | Speedup | Tests |
|---|---:|---:|---:|---:|
| carbon | 21.6s | 8.8s | **2.5×** | 6 027 |
| doctrine-orm | 1.61s | 1.63s | tied | 3 438 |
| faker | 1.03s | 0.83s | **1.25×** | 1 380 |
| php-parser | 0.40s | 0.36s | 1.10× | 1 887 |
| guzzle-psr7 | 0.14s | 0.20s | — | 1 087 |
| doctrine-collections | <0.05s | 0.12s | — | 143 |

CPU-bound suites with many independent classes (carbon especially) see the
largest gains. Sub-second suites are dominated by PHP startup cost and the
fork-pool overhead doesn't amortize; for those, prefer
`--workers 1` or run vanilla.

For a project requiring PHP 8.4 (e.g. PHPUnit's own test suite), a Docker
harness in `bench/Dockerfile.php84` builds a container with `pcntl`
pre-compiled. A single-run smoke on phpunit-itself showed vanilla ~74s
vs phpunit-rust ~3s (median validation pending — fixture-loading quirks
in PHPUnit's own tests cause a partial test-count gap).

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
