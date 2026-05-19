# phpunit-rust

A Rust orchestrator that runs PHPUnit tests in parallel using plain PHP CLI
workers — no FrankenPHP, no HTTP, no daemon. Workers communicate over
stdin/stdout with a one-JSON-per-line protocol and delegate actual test
execution to the project's own PHPUnit installation.

## Status: v0.6.0 — plain PHP CLI workers + per-row parallelism

phpunit-rust spawns N plain `php` processes, distributes test classes (and
individual data-provider rows) across them via Rayon, and streams results back.
Workers are long-lived within a run and restart only on crash.

### Supported

- All `TestCase` assertions — worker delegates to the project's PHPUnit
- Mocks (`createMock`, `MockBuilder`, expectation chaining)
- `expectException`, `expectExceptionMessage`, `expectExceptionCode`
- Data providers (`#[DataProvider]` / `@dataProvider`) — per-row parallelism
- Test dependencies (`#[Depends]`, with return-value passing within a class)
- `markTestSkipped`, `markTestIncomplete`
- `setUp` / `tearDown` per test; `setUpBeforeClass` / `tearDownAfterClass`
- `phpunit.xml` — `bootstrap`, `<testsuites>` directories/excludes, `<php><const>`
- `--bootstrap`, `--filter`, `--workers`, `--configuration` CLI flags
- Test discovery: `testXxx` naming, `@test` PHPDoc annotation, `#[Test]` attribute
- Fully-qualified extends (`extends \PHPUnit\Framework\TestCase`)
- Abstract base classes outside testsuite dirs resolved via `composer.json` autoload

### Not yet supported (deferred)

- Smart scheduling (longest-first, fail-fast)
- `@runInSeparateProcess`
- Code coverage (PCOV/Xdebug)
- JUnit XML / TAP / TestDox reporters
- Watch mode
- Custom PHPUnit extensions/listeners
- Risky test detection (no assertions, unexpected output, etc.)

## Requirements

- Rust 1.75+
- PHP 8.1+ on `$PATH`
- Project under test must have `composer install`'d; PHPUnit 10 or 11 on its
  vendor path. Tested against PHPUnit 10.5 and 11.5.

## Usage

```bash
cargo build --release

# auto-detect phpunit.xml, use all CPU cores
./target/release/phpunit-rust --project /path/to/php/project

# explicit worker count
./target/release/phpunit-rust --project /path/to/php/project --workers 8

# sequential (no parallelism overhead — best for tiny suites)
./target/release/phpunit-rust --project /path/to/php/project --workers 1

# filter by class or method name
./target/release/phpunit-rust --project /path/to/php/project --filter MyClass

# explicit bootstrap
./target/release/phpunit-rust --project /path/to/php/project --bootstrap tests/bootstrap.php
```

`phpunit.xml` / `phpunit.xml.dist` are auto-detected at the project root.
`--configuration` overrides the search path. The runner reads `bootstrap`,
`<testsuites>` directories/excludes, and `<php><const>` entries. The `--tests-dir`
flag is the fallback when no `phpunit.xml` is found.

## Architecture

```
phpunit-rust (Rust binary)
  ├─ discovery   : tree-sitter-php; class graph + BFS for transitive inheritance
  ├─ phpunit_xml : parser for bootstrap, <testsuites>, <php><const>
  ├─ php_worker  : PhpWorkerPool — N plain `php` processes, stdin/stdout JSON
  ├─ runner      : rayon::par_iter distributes classes + data-provider rows
  └─ reporter    : TTY progress + summary (thread-safe)

N PHP workers (plain `php` CLI, long-lived per run)
  └─ worker.php → require vendor/autoload.php → PHPUnit TestCase::runTest()
```

Workers are plain `php worker.php` processes. Each receives a JSON job on
stdin and writes a JSON result to stdout. No HTTP, no socket, no daemon.

## Performance

Benchmarked on PHP 8.4 with 4 workers against a selection of real OSS suites.
"vanilla-phpunit" is a single-process `./vendor/bin/phpunit` run.

| Project | vanilla | phpunit-rust (4w) | Speedup | Tests |
|---|---|---|---|---|
| carbon | 0:23.10 | 0:09.90 | **2.3×** | 6 027 |
| ramsey-uuid | 0:06.19 | 0:02.10 | **2.9×** | 2 022 |
| doctrine-orm | 0:04.10 | 0:01.72 | **2.4×** | 3 478 |
| faker | 0:00.88 | 0:00.81 | 1.1× | 1 380 |
| php-parser | 0:00.38 | 0:00.30 | 1.3× | 1 887 |
| guzzle-psr7 | 0:00.12 | 0:00.15 | — | 1 088 |
| doctrine-collections | 0:00.09 | 0:00.10 | — | 242 |

CPU-bound suites with many independent classes (carbon, ramsey-uuid,
doctrine-orm) see the largest gains. Sub-second suites are dominated by PHP
startup cost and show parity or slight regression at low worker counts.

The bench script (`bench/run.sh`) runs the full matrix across PHP 8.1–8.5 in
Docker containers. `--quick` restricts it to PHP 8.4 + 4 workers for fast
iteration.
