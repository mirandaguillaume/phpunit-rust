# phpunit-rust

A Rust test runner that orchestrates FrankenPHP to execute PHPUnit-style tests
using its own minimal test executor — no longer dependent on PHPUnit's internal
`TestRunner` API.

## Status: v0.3.0 — own the runner

phpunit-rust no longer drives PHPUnit's `TestRunner`. Instead, it ships its
own `PhpunitRust\TestExecutor` that calls into PHPUnit's *stable* user-facing
surface only (TestCase, assertions, mocks, marker exceptions). Result: we no
longer track PHPUnit's runner internals across versions, and the PHP-side
codebase is ~70% smaller.

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

### Not yet supported (deferred)

- Parallel execution (separate follow-up plan)
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
  ├─ frankenphp  : spawns FrankenPHP child in worker mode; HTTP readiness probe
  ├─ client      : POSTs TestRunRequest, parses {outcomes:[…]}
  ├─ runner      : one HTTP request per test class
  └─ reporter    : TTY output (Pass/Fail/Error/Skip/Incomplete/Risky)

worker.php (long-lived in FrankenPHP worker mode, ~50 lines)
  └─ TestExecutor::runClass(class, methods)
        ├─ MethodPlanner: expand DataProvider rows; topological sort by Depends
        ├─ Loop each step:
        │    ├─ instantiate test class
        │    ├─ Closure-bound setUp()
        │    ├─ invoke test method with depends-args + provider-args
        │    ├─ Closure-bound tearDown()
        │    └─ classify Throwable via OutcomeBuilder
        └─ return list of outcome dicts as JSON
```
