# phpunit-rust

A Rust test runner that orchestrates FrankenPHP to execute PHPUnit tests.

## Status: v0.2.0 — real PHPUnit runner integration

phpunit-rust now drives PHPUnit's actual `TestRunner` and Event Facade.
Supported in this release:

- `expectException`, `expectExceptionMessage`, `expectExceptionCode`
- Data providers (`#[DataProvider]`, `@dataProvider`) — each row reported separately
- Test dependencies (`#[Depends]`, `@depends`) — value passing across tests
- `markTestSkipped`, `markTestIncomplete`, "risky" detection
- `phpunit.xml` auto-detection (or `--configuration` flag) with `bootstrap`
  file loading and `<php>` block application

Still deferred to follow-up plans:

- Parallel execution (worker pool + work-stealing scheduler)
- `@runInSeparateProcess`
- Code coverage (PCOV/Xdebug)
- JUnit XML / TAP / TestDox reporters for CI
- Watch mode
- Custom PHPUnit extensions and listeners

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
./target/release/phpunit-rust --project /path/to/php/project --configuration phpunit.ci.xml
```

If `--configuration` is omitted, the runner auto-detects `phpunit.xml` then
`phpunit.xml.dist` at the project root.

## Architecture

The Rust binary spawns FrankenPHP as a long-running worker, then makes HTTP POST requests to `/worker.php` with a JSON body containing:

1. Autoload path (vendor/autoload.php)
2. PHPUnit configuration (phpunit.xml, bootstrap)
3. Test file, class, and methods to run

The worker:

1. Bootstraps PHP and loads PHPUnit
2. Registers a custom `ResultCollector` subscriber on the Event Facade
3. Calls `TestRunner::run()` with a test suite containing only the requested methods
4. Collects test outcomes (pass, fail, skipped, incomplete, risky) including exception messages and dataset info
5. Returns JSON-encoded outcomes to the Rust runner

The Rust runner aggregates outcomes by status, formats them, and prints a summary.

## Wire protocol

Request (`POST /worker.php`):
```json
{
  "autoload": "/path/to/project/vendor/autoload.php",
  "phpunit_xml": "/path/to/project/phpunit.xml",
  "file": "/path/to/project/tests/FooTest.php",
  "class": "App\\Tests\\FooTest",
  "methods": ["testBar", "testBaz"]
}
```

Response:
```json
{
  "outcomes": [
    {"class":"App\\Tests\\FooTest","method":"testBar","dataset":null,"status":"pass","message":null,"trace":null,"duration_ms":1.2},
    {"class":"App\\Tests\\FooTest","method":"testBaz","dataset":"#0","status":"fail","message":"…","trace":"…","duration_ms":0.3}
  ]
}
```
