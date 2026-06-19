# phpunit-rust

A Rust orchestrator that runs PHPUnit tests in parallel using forked PHP
workers — no FrankenPHP, no HTTP, no daemon. One PHP master loads the
project's autoloader and bootstrap once; N children are forked via
`pcntl_fork()` so they inherit the warmed-up interpreter via copy-on-write.
Tests run against the project's own PHPUnit — its `TestCase` API, assertions
and mocks — but driven by a thin in-house lifecycle shim (`TestExecutor`),
**not** PHPUnit's own `TestRunner`/`Facade`. That re-implementation is why a
handful of behavioural edges differ from vanilla (see [COMPATIBILITY.md](COMPATIBILITY.md)).

## Status: v0.8.0 — exact test-count parity with vanilla PHPUnit on all 8 benchmarked suites

The runner spawns one PHP master, forks N children, then streams test
classes (and individual data-provider rows for heavy providers) over
per-child pipes using a work-stealing queue. LPT (longest-processing-
time-first) scheduling means the heaviest classes start on all workers
concurrently instead of stranding one at the end.

## Highlights

- **Work-stealing + LPT scheduling** — heavy classes start on all workers concurrently; fork-pool startup is ~50 ms.
- **Exact test-count parity** with vanilla PHPUnit on all 8 benchmarked suites (brick-math, carbon, commonmark, doctrine-orm, faker, guzzle-psr7, php-parser, and PHPUnit's own `unit` testsuite) — see [BENCHMARKS.md](BENCHMARKS.md).
- **Up to 2.9× faster** on CPU-bound suites at 4 workers (brick-math; carbon 1.6×, more at higher worker counts); DB-less functional suites (doctrine-orm) are about tied, and sub-second suites run faster with vanilla.
- **Static coverage** via the `analyzer` crate (mago AST + per-test attribution; no Xdebug / PCOV needed).
- **Reliable shutdown** — SIGINT kills the PHP master and every forked child via `PR_SET_PDEATHSIG`; no orphan workers.

## Requirements

- Rust 1.75+
- PHP 8.1+ with the `pcntl` extension on `$PATH` (Linux/macOS;
  Windows not supported — `pcntl_fork()` is POSIX-only)
- Project under test: `composer install`'d, PHPUnit 10 or 11 on its
  vendor path. Tested against PHPUnit 10.5 and 11.5.

## Usage

```bash
cargo build --release

# one-time: install the PHP worker shim's autoloader
# (the forked workers `require` php/vendor/autoload.php at startup)
composer install --no-dev --working-dir=php

# auto-detect phpunit.xml; workers default to the number of detected CPU cores
./target/release/phpunit-rust --project /path/to/php/project

# explicit worker count (honored verbatim; default = detected CPU cores)
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

## Documentation

- [Architecture](ARCHITECTURE.md) — the fork-server / worker-pool design
- [Compatibility](COMPATIBILITY.md) — supported PHPUnit features & what's deferred
- [Benchmarks](BENCHMARKS.md) — performance vs vanilla PHPUnit + how to run the bench

## License

[MIT](LICENSE) © 2026 Guillaume Miranda
