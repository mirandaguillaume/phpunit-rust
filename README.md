# phpunit-rust

A Rust test runner that orchestrates FrankenPHP to execute PHPUnit tests.

## Status: MVP (early)

Sequential execution only. Direct test-method invocation — **does not** use PHPUnit's full `TestRunner`, so the following are NOT yet supported:

- `expectException` / `@expectedException` (tests using these will report as `error`)
- Data providers (`#[DataProvider]`, `@dataProvider`)
- Test dependencies (`@depends`)
- `@runInSeparateProcess`
- Code coverage
- `phpunit.xml` configuration
- Custom listeners/extensions

All of these are scoped to follow-up work. See `docs/superpowers/plans/`.

## Requirements

- Rust 1.75+
- FrankenPHP 1.x on `$PATH`
- PHP 8.1+ on the system, PHPUnit 10.x in the project under test (FrankenPHP supplies its own bundled PHP at runtime)
- `composer install` must have been run in both the project under test and this repo's `php/` directory

## Usage

```bash
cargo build --release
./target/release/phpunit-rust --project /path/to/php/project
./target/release/phpunit-rust --project /path/to/php/project --filter MyClass
```

## Architecture

```
phpunit-rust (Rust binary)
  ├─ discovery   : tree-sitter-php parses test files
  ├─ frankenphp  : spawns FrankenPHP child process in worker mode
  ├─ client      : HTTP/JSON to worker.php
  ├─ runner      : sequential orchestration
  └─ reporter    : TTY output

worker.php (long-lived, in FrankenPHP worker mode)
  ├─ loads project autoloader once (cached across requests)
  ├─ for each request: requires test file, instantiates test class,
  │  invokes method, captures assertions/exceptions
  └─ returns JSON outcome
```
