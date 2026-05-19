# phpunit-rust

A Rust test runner that orchestrates FrankenPHP to execute PHPUnit-style tests
using its own minimal test executor — no longer dependent on PHPUnit's internal
`TestRunner` API.

## Status: v0.4.0 — parallel execution

phpunit-rust now runs N FrankenPHP worker processes concurrently. Test
classes are distributed across the pool via Rayon. Wall-clock speedup
scales with CPU count for CPU-bound suites, bounded by the largest
single class (per-method parallelism is a future plan).

### Supported

- `assertSame` / `assertEquals` / all `TestCase` assertions (we don't touch them — user code calls them on `$this`)
- Mocks (`createMock`, `MockBuilder`, expectation chaining — same)
- `expectException`, `expectExceptionMessage`, `expectExceptionCode`
- Data providers (`#[DataProvider]`, expanded to one outcome per row)
- Test dependencies (`#[Depends]`, with return-value passing within a class)
- `markTestSkipped`, `markTestIncomplete`
- `setUp` / `tearDown` per test
- `setUpBeforeClass` / `tearDownAfterClass` per class run
- `phpunit.xml`'s `bootstrap` attribute (or `--bootstrap` flag)
- `--workers N` for parallel class-level dispatch (defaults to num_cpus)

### Not yet supported (deferred)

- Parallelism *within* a single test class (a single huge class is currently single-threaded)
- Smart scheduling (longest-first, fail-fast-first, history-based)
- `@runInSeparateProcess`
- Code coverage (PCOV/Xdebug)
- JUnit XML / TAP / TestDox reporters
- Watch mode
- Custom PHPUnit extensions/listeners
- Risky test detection (no assertions, unexpected output, etc.)

## Requirements

- Rust 1.75+
- FrankenPHP 1.x on `$PATH`
- PHP 8.1+ on the system
- Project under test must have `composer install`'d a PHPUnit version that
  provides `PHPUnit\Framework\TestCase` and the assertion/mock surface.
  Tested against PHPUnit 10.5 and 11.5.

## Usage

```bash
cargo build --release
./target/release/phpunit-rust --project /path/to/php/project
./target/release/phpunit-rust --project /path/to/php/project --workers 8
./target/release/phpunit-rust --project /path/to/php/project --workers 1     # sequential
./target/release/phpunit-rust --project /path/to/php/project --filter MyClass
./target/release/phpunit-rust --project /path/to/php/project --bootstrap tests/bootstrap.php
```

If `--configuration` is omitted, the runner auto-detects `phpunit.xml` then
`phpunit.xml.dist` at the project root and reads only the `bootstrap`
attribute. All other `phpunit.xml` settings are ignored (use CLI flags
instead).

## Architecture

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

## Performance

Real measurements on a 22-CPU machine running the [brick/math](https://github.com/brick/math)
test suite (13,589 tests via PHPUnit 11):

| Workers | Wall-clock | Speedup |
|---------|-----------|---------|
| 1       | 234s (3:54) | 1.0× (baseline) |
| 22 (`num_cpus` default) | 188s (3:08) | **1.24×** |

The speedup ceiling depends on the **largest single class** in the suite.
brick/math's `BigIntegerTest` takes ~165s alone — one CPU is pinned to it
for that entire duration, while the other 21 workers finish their classes
in seconds. Suites with more even per-class duration distributions see
proportionally larger speedups; suites with one dominant class approach
`max(per_class_time)` as their wall-clock floor.

For very small suites, parallel mode is *slower* than sequential because
FrankenPHP startup (~1.5s per worker) dominates:

| Workers | 15-test fixture |
|---------|-----------------|
| 1       | 1.15s |
| 4       | 2.50s |

Use `--workers 1` for small suites or quick smoke runs.
