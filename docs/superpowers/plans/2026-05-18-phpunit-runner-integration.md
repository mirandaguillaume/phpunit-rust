# PHPUnit Runner Integration Plan (v0.2.0)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the MVP's direct test-method invocation with PHPUnit 10's real `TestRunner` so that `expectException`, data providers, `@depends`, `markTestSkipped`, `markTestIncomplete`, "risky" detection, and `phpunit.xml` bootstrap all work — enabling phpunit-rust to run real-world PHPUnit suites.

**Architecture:** The worker stops invoking test methods by hand and starts driving `PHPUnit\TextUI\TestRunner` against `TestSuite::fromClassName($class)`. A long-lived `ResultCollector` (implementing all outcome subscriber interfaces) is registered with `PHPUnit\Event\Facade::instance()` once at worker boot. Per request, the collector is `reset()`, internal singletons (`PassedTests`) are nulled via reflection, the suite is built and run, and the collector's accumulated outcomes are returned. The dispatch model changes from "one request per test method" to "one request per test class" — letting PHPUnit handle method ordering, data-provider expansion, and dependency resolution within the class. The wire protocol breaks: `TestOutcome` becomes `Vec<TestOutcome>`, each carrying an optional `dataset` field. `phpunit.xml` is honored if present at the project root.

**Tech Stack:**
- Existing: Rust 1.75+, FrankenPHP 1.x (bundled PHP 8.5), PHPUnit 10.5
- New PHPUnit 10 APIs in use:
  - `PHPUnit\Framework\TestSuite::fromClassName()` — suite construction
  - `PHPUnit\TextUI\TestRunner::run($config, $resultCache, $suite)` — execution
  - `PHPUnit\TextUI\Configuration\Registry::init($cli, $xml)` — config registration
  - `PHPUnit\TextUI\XmlConfiguration\Loader::load()` — phpunit.xml parsing
  - `PHPUnit\TextUI\Configuration\PhpHandler::handle()` — `<php>` block application
  - `PHPUnit\Event\Facade::instance()->registerSubscriber()` — event subscription
  - Event types: `Test\Passed`, `Test\Failed`, `Test\Errored`, `Test\Skipped`, `Test\MarkedIncomplete`, `Test\ConsideredRisky`
  - `PHPUnit\Runner\ResultCache\NullResultCache` — disables caching
  - `PHPUnit\TestRunner\TestResult\PassedTests` — singleton for `@depends` (resets via reflection)

**Verification notes for the implementer (read before starting):**
- **NEVER call `PHPUnit\Event\Facade::instance()->seal()`** — it makes subscriber registration throw forever. The normal PHPUnit CLI flow seals, but our worker must not.
- **The `Registry` singleton accepts `Registry::init()` repeatedly** — it replaces the active configuration. Safe to call per request.
- **`PassedTests` has no public reset.** Use reflection on its private static `$instance` to null it between requests. Without this, `@depends` lookups across requests will return stale matches.
- **TestSuite instances can't be reused.** Each request must call `TestSuite::fromClassName()` fresh.
- **PHPUnit 10.5 paths.** All API references are valid in `/home/gumiranda/PHPUnit_rust/php/vendor/phpunit/phpunit/`. If a class moves in a future PHPUnit version, this plan needs revisiting.

---

## File Structure

```
src/types.rs              # CHANGE: add Skipped variant; TestOutcome.dataset; new TestRunRequest
src/discovery.rs          # ADD: group_by_class() helper; TestClass struct
src/client.rs             # CHANGE: run_class(req) -> Vec<TestOutcome>
src/runner.rs             # CHANGE: dispatch by class; aggregate outcomes
src/reporter.rs           # CHANGE: render Skipped status + dataset label
src/main.rs               # CHANGE: pass --configuration through; new dispatch shape
php/worker.php            # REWRITE: drive TestRunner, manage state
php/src/ResultCollector.php       # NEW: subscriber that collects outcomes
php/src/Bootstrap.php             # NEW: pure-function helpers for config + reset
php/composer.json         # CHANGE: add psr-4 autoload for php/src/
fixtures/sample_project/src/Repository.php           # NEW: subject with @depends-chain example
fixtures/sample_project/tests/DataProviderTest.php   # NEW: exercises #[DataProvider]
fixtures/sample_project/tests/DependsTest.php        # NEW: exercises @depends value passing
fixtures/sample_project/tests/SkippedTest.php        # NEW: exercises skipped + incomplete
tests/integration.rs      # CHANGE: cover the new outcomes
docs/superpowers/plans/2026-05-18-phpunit-runner-integration.md   # this plan
```

**Boundaries:**
- `ResultCollector.php` is the only place that knows about PHPUnit's event subscribers. `Bootstrap.php` is the only place that knows about `Registry::init`/`PhpHandler`/`PassedTests` reset. `worker.php` orchestrates: bootstrap → reset → build suite → run → return collector outcomes.
- Rust side: `client.rs` owns the wire (Vec deserialization). `runner.rs` owns batching (group by class, dispatch once per class).

---

## Task 1: Wire types — add Skipped, dataset, class-based request

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/types.rs`

- [ ] **Step 1: Replace TestStatus to add Skipped**

In `src/types.rs`, change the `TestStatus` enum to:

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    Pass,
    Fail,
    Error,
    Skipped,
    Incomplete,
    Risky,
}
```

- [ ] **Step 2: Extend TestOutcome with optional dataset**

Replace the `TestOutcome` struct with:

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TestOutcome {
    pub class: String,
    pub method: String,
    /// PHPUnit data-provider row identifier, e.g. "0" or "with strings".
    /// `None` for tests that aren't parameterized.
    #[serde(default)]
    pub dataset: Option<String>,
    pub status: TestStatus,
    pub message: Option<String>,
    pub trace: Option<String>,
    pub duration_ms: f64,
}
```

- [ ] **Step 3: Add TestRunRequest (replaces TestRequest)**

Replace the `TestRequest` struct with `TestRunRequest`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestRunRequest {
    pub autoload: PathBuf,
    /// Path to phpunit.xml if the user has one. None → use PHPUnit defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phpunit_xml: Option<PathBuf>,
    pub file: PathBuf,
    pub class: String,
    /// Empty vec means "run all test methods in the class".
    pub methods: Vec<String>,
}
```

- [ ] **Step 4: Update tests to match**

Replace the existing `#[cfg(test)] mod tests` block with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_request_omits_phpunit_xml_when_none() {
        let req = TestRunRequest {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            phpunit_xml: None,
            file: PathBuf::from("/p/tests/Foo.php"),
            class: "App\\Tests\\FooTest".into(),
            methods: vec![],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("phpunit_xml").is_none());
        // serde_json::to_value returns a Value, not a serialized string, so
        // backslashes are NOT JSON-escaped here. Compare to the raw string.
        assert_eq!(json["class"], "App\\Tests\\FooTest");
    }

    #[test]
    fn run_request_includes_phpunit_xml_when_present() {
        let req = TestRunRequest {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            phpunit_xml: Some(PathBuf::from("/p/phpunit.xml")),
            file: PathBuf::from("/p/tests/Foo.php"),
            class: "FooTest".into(),
            methods: vec!["testBar".into()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["phpunit_xml"], "/p/phpunit.xml");
        assert_eq!(json["methods"][0], "testBar");
    }

    #[test]
    fn outcome_deserializes_with_dataset() {
        let raw = r#"{"class":"A","method":"b","dataset":"with strings","status":"pass","message":null,"trace":null,"duration_ms":1.0}"#;
        let outcome: TestOutcome = serde_json::from_str(raw).unwrap();
        assert_eq!(outcome.dataset.as_deref(), Some("with strings"));
        assert_eq!(outcome.status, TestStatus::Pass);
    }

    #[test]
    fn outcome_deserializes_without_dataset() {
        let raw = r#"{"class":"A","method":"b","status":"skipped","message":"reason","trace":null,"duration_ms":0.0}"#;
        let outcome: TestOutcome = serde_json::from_str(raw).unwrap();
        assert!(outcome.dataset.is_none());
        assert_eq!(outcome.status, TestStatus::Skipped);
    }

    #[test]
    fn outcome_deserializes_all_new_statuses() {
        for (raw_status, expected) in [
            ("pass", TestStatus::Pass),
            ("fail", TestStatus::Fail),
            ("error", TestStatus::Error),
            ("skipped", TestStatus::Skipped),
            ("incomplete", TestStatus::Incomplete),
            ("risky", TestStatus::Risky),
        ] {
            let raw = format!(r#"{{"class":"A","method":"b","status":"{}","message":null,"trace":null,"duration_ms":0.0}}"#, raw_status);
            let outcome: TestOutcome = serde_json::from_str(&raw).unwrap();
            assert_eq!(outcome.status, expected);
        }
    }
}
```

- [ ] **Step 5: Verify the test code compiles in isolation (syntax check only)**

`cargo test --lib types` would normally run the types tests, but it can't here: the lib won't compile because (a) `client.rs`, `runner.rs`, `main.rs`, `tests/integration.rs` still reference the removed `TestRequest`, **and (b)** `reporter.rs` has a `match` on `TestStatus` that becomes non-exhaustive once we add Skipped/Incomplete/Risky.

Run instead:

```bash
cd /home/gumiranda/PHPUnit_rust && cargo check --lib --tests --no-default-features 2>&1 | grep -E "src/types.rs" | head -5
```

Expected: no errors mentioning `src/types.rs` (types.rs itself is clean; other files' errors are expected and resolved by Tasks 6–10).

The 5 types-tests will actually run after Task 8 (reporter update). They're logically verifiable now from reading the code, but `cargo test` requires the whole lib to compile.

**Don't fix the errors in other files in this task** — Tasks 6–10 do that, in coordinated wire-change order.

- [ ] **Step 6: Build status check**

```bash
cd /home/gumiranda/PHPUnit_rust && cargo build 2>&1 | grep -E "^error" | head -10
```

Expected: compile errors in `client.rs`, `runner.rs`, `main.rs`, `tests/integration.rs` (`TestRequest` unresolved) **plus** `reporter.rs` (non-exhaustive `match TestStatus`). All resolved by Tasks 6–10.

- [ ] **Step 7: Commit**

```bash
git add src/types.rs
git commit -m "feat(types): add Skipped/Incomplete/Risky statuses, dataset field, TestRunRequest"
```

---

## Task 2: Discovery — group tests by class

The new dispatch model sends one request per class. Discovery still returns per-method `TestCase`s (so existing CLI filters keep working), but the runner needs a helper to group them.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/discovery.rs`

- [ ] **Step 1: Add a TestClass struct and grouping function**

At the top of `src/discovery.rs`, after the existing imports and before `discover_in_file`, add:

```rust
/// A discovered test class with all of its methods, grouped for batched
/// dispatch (one request per class to the worker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestClass {
    pub file: PathBuf,
    pub class: String,
    pub methods: Vec<String>,
}

/// Group a flat list of TestCases by class. Preserves discovery order.
pub fn group_by_class(cases: Vec<TestCase>) -> Vec<TestClass> {
    let mut groups: Vec<TestClass> = Vec::new();
    for case in cases {
        if let Some(existing) = groups.iter_mut().find(|g| g.class == case.class) {
            existing.methods.push(case.method);
        } else {
            groups.push(TestClass {
                file: case.file,
                class: case.class,
                methods: vec![case.method],
            });
        }
    }
    groups
}
```

- [ ] **Step 2: Add a unit test**

Add to the `mod tests` block in `src/discovery.rs`:

```rust
    #[test]
    fn group_by_class_collapses_per_method_cases() {
        let cases = vec![
            TestCase { file: PathBuf::from("/p/A.php"), class: "A".into(), method: "testOne".into() },
            TestCase { file: PathBuf::from("/p/A.php"), class: "A".into(), method: "testTwo".into() },
            TestCase { file: PathBuf::from("/p/B.php"), class: "B".into(), method: "testThree".into() },
        ];
        let grouped = group_by_class(cases);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].class, "A");
        assert_eq!(grouped[0].methods, vec!["testOne".to_string(), "testTwo".to_string()]);
        assert_eq!(grouped[1].class, "B");
        assert_eq!(grouped[1].methods, vec!["testThree".to_string()]);
    }
```

(You will need `use std::path::PathBuf;` in the test module — already imported.)

- [ ] **Step 3: Run the test**

```bash
cd /home/gumiranda/PHPUnit_rust && cargo test --lib discovery
```

Expected: `test result: ok. 5 passed; 0 failed` (4 prior + 1 new).

- [ ] **Step 4: Commit**

```bash
git add src/discovery.rs
git commit -m "feat(discovery): add TestClass + group_by_class for batched dispatch"
```

---

## Task 3: PHP worker autoload + ResultCollector subscriber

We add `php/src/` for our worker-side PHP code and a Composer PSR-4 autoload so `worker.php` can `use \PhpunitRust\ResultCollector`.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/php/composer.json`
- Create: `/home/gumiranda/PHPUnit_rust/php/src/ResultCollector.php`

- [ ] **Step 1: Add autoload to `php/composer.json`**

Replace `/home/gumiranda/PHPUnit_rust/php/composer.json` with:

```json
{
    "name": "phpunit-rust/worker",
    "type": "project",
    "description": "FrankenPHP worker that executes PHPUnit tests on behalf of phpunit-rust",
    "autoload": {
        "psr-4": {
            "PhpunitRust\\": "src/"
        }
    },
    "require": {
        "php": ">=8.1",
        "phpunit/phpunit": "^10.5"
    }
}
```

- [ ] **Step 2: Regenerate the autoloader**

```bash
cd /home/gumiranda/PHPUnit_rust/php && composer dump-autoload --no-interaction
```

Expected: `Generating autoload files` + `Generated autoload files`.

- [ ] **Step 3: Write `ResultCollector.php`**

Write `/home/gumiranda/PHPUnit_rust/php/src/ResultCollector.php`:

```php
<?php

declare(strict_types=1);

namespace PhpunitRust;

use PHPUnit\Event\Test\ConsideredRisky;
use PHPUnit\Event\Test\ConsideredRiskySubscriber;
use PHPUnit\Event\Test\Errored;
use PHPUnit\Event\Test\ErroredSubscriber;
use PHPUnit\Event\Test\Failed;
use PHPUnit\Event\Test\FailedSubscriber;
use PHPUnit\Event\Test\Finished;
use PHPUnit\Event\Test\FinishedSubscriber;
use PHPUnit\Event\Test\MarkedIncomplete;
use PHPUnit\Event\Test\MarkedIncompleteSubscriber;
use PHPUnit\Event\Test\Passed;
use PHPUnit\Event\Test\PassedSubscriber;
use PHPUnit\Event\Test\PreparationStarted;
use PHPUnit\Event\Test\PreparationStartedSubscriber;
use PHPUnit\Event\Test\Skipped;
use PHPUnit\Event\Test\SkippedSubscriber;
use PHPUnit\Event\Value\Test\TestMethod;

/**
 * Long-lived subscriber registered with PHPUnit's Facade once at worker boot.
 * Collects outcomes for the *current* request; reset() must be called by the
 * worker between requests.
 *
 * We implement multiple subscriber interfaces on one object so a single
 * registration suffices. PHPUnit's event dispatcher routes by the typed
 * `notify` parameter, so the right method fires for each event.
 */
final class ResultCollector implements
    PassedSubscriber,
    FailedSubscriber,
    ErroredSubscriber,
    SkippedSubscriber,
    MarkedIncompleteSubscriber,
    ConsideredRiskySubscriber,
    PreparationStartedSubscriber,
    FinishedSubscriber
{
    /** @var array<int, array{class:string,method:string,dataset:?string,status:string,message:?string,trace:?string,duration_ms:float}> */
    private array $outcomes = [];

    /** @var array<string, float> Map of TestMethod::id() → start microtime */
    private array $startTimes = [];

    /** @var array<string, string> Map of TestMethod::id() → outcome status already recorded */
    private array $recorded = [];

    public function reset(): void
    {
        $this->outcomes = [];
        $this->startTimes = [];
        $this->recorded = [];
    }

    /** @return list<array<string, mixed>> */
    public function outcomes(): array
    {
        return $this->outcomes;
    }

    public function notify(/* one of the event types above */ $event): void
    {
        // PHPUnit's dispatcher calls one of the typed notify(...) overloads
        // we declare below by interface. PHP doesn't support real method
        // overloading, so we route by event type here.
        if ($event instanceof PreparationStarted) {
            $this->startTimes[$event->test()->id()] = microtime(true);
            return;
        }
        if ($event instanceof Passed) {
            $this->record($event->test(), 'pass', null, null);
            return;
        }
        if ($event instanceof Failed) {
            $this->record($event->test(), 'fail', $event->throwable()->message(), $event->throwable()->stackTrace());
            return;
        }
        if ($event instanceof Errored) {
            $this->record($event->test(), 'error', $event->throwable()->message(), $event->throwable()->stackTrace());
            return;
        }
        if ($event instanceof Skipped) {
            $this->record($event->test(), 'skipped', $event->message(), null);
            return;
        }
        if ($event instanceof MarkedIncomplete) {
            $this->record($event->test(), 'incomplete', $event->throwable()->message(), $event->throwable()->stackTrace());
            return;
        }
        if ($event instanceof ConsideredRisky) {
            // Risky can fire multiple times per test; only record first.
            if (!isset($this->recorded[$event->test()->id()])) {
                $this->record($event->test(), 'risky', $event->message(), null);
            }
            return;
        }
        if ($event instanceof Finished) {
            // If we got here with no outcome recorded, the test was prepared
            // but never produced an outcome event — synthesize an error.
            $id = $event->test()->id();
            if (!isset($this->recorded[$id]) && $event->test() instanceof TestMethod) {
                $this->record($event->test(), 'error', 'no outcome reported by PHPUnit', null);
            }
            return;
        }
    }

    private function record($test, string $status, ?string $message, ?string $trace): void
    {
        if (!$test instanceof TestMethod) {
            return;
        }
        $id = $test->id();
        if (isset($this->recorded[$id])) {
            return; // first wins (e.g., Failed before Risky)
        }
        $this->recorded[$id] = $status;

        $start = $this->startTimes[$id] ?? microtime(true);
        $duration = (microtime(true) - $start) * 1000.0;

        $dataset = null;
        $testData = $test->testData();
        if ($testData->hasDataFromDataProvider()) {
            $name = $testData->dataFromDataProvider()->dataSetName();
            $dataset = is_int($name) ? "#{$name}" : $name;
        }

        $this->outcomes[] = [
            'class'       => $test->className(),
            'method'      => $test->methodName(),
            'dataset'     => $dataset,
            'status'      => $status,
            'message'     => $message,
            'trace'       => $trace,
            'duration_ms' => $duration,
        ];
    }
}
```

**Why one `notify()` with manual dispatch:** PHPUnit's subscriber interfaces each declare their own typed `notify(EventType $event): void` method. PHP would normally require N separate methods. Implementing all interfaces on one class works because each interface contributes the same method name — they coexist as long as the parameter is the union. The single-method routing keeps the file focused.

- [ ] **Step 4: Smoke-test syntax**

```bash
cd /home/gumiranda/PHPUnit_rust/php && php -l src/ResultCollector.php
```

Expected: `No syntax errors detected in src/ResultCollector.php`.

- [ ] **Step 5: Commit**

```bash
cd /home/gumiranda/PHPUnit_rust && git add php/composer.json php/src/ResultCollector.php
git commit -m "feat(worker): add ResultCollector subscriber for PHPUnit events"
```

---

## Task 4: PHPUnit bootstrap helper

A pure helper class that handles config loading (defaults or phpunit.xml), `<php>` block application, bootstrap file requirement, and per-request state reset.

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/php/src/Bootstrap.php`

- [ ] **Step 1: Write `Bootstrap.php`**

Write `/home/gumiranda/PHPUnit_rust/php/src/Bootstrap.php`:

```php
<?php

declare(strict_types=1);

namespace PhpunitRust;

use PHPUnit\TestRunner\TestResult\PassedTests;
use PHPUnit\TextUI\CliArguments\Builder as CliBuilder;
use PHPUnit\TextUI\Configuration\PhpHandler;
use PHPUnit\TextUI\Configuration\Registry;
use PHPUnit\TextUI\XmlConfiguration\DefaultConfiguration;
use PHPUnit\TextUI\XmlConfiguration\Loader as XmlLoader;
use PHPUnit\TextUI\Configuration\Configuration;

/**
 * Per-request PHPUnit bootstrap. Builds a Configuration, registers it,
 * applies <php> block and bootstrap file, and resets PHPUnit's hidden
 * singletons so the worker can run thousands of suites cleanly.
 */
final class Bootstrap
{
    /** Keep track of bootstrap files we've already required to avoid double-require errors. */
    private static array $bootstrapsLoaded = [];

    public static function configure(?string $phpunitXmlPath): Configuration
    {
        if ($phpunitXmlPath !== null && is_file($phpunitXmlPath)) {
            $xmlConfig = (new XmlLoader)->load($phpunitXmlPath);
        } else {
            $xmlConfig = DefaultConfiguration::create();
        }
        $cliConfig = (new CliBuilder)->fromParameters([]);
        $config    = Registry::init($cliConfig, $xmlConfig);

        // Apply <php> block (ini settings, env, constants).
        (new PhpHandler)->handle($config->php());

        // Apply the bootstrap file once per worker process.
        if ($config->hasBootstrap()) {
            $path = $config->bootstrap();
            if (!isset(self::$bootstrapsLoaded[$path])) {
                require $path;
                self::$bootstrapsLoaded[$path] = true;
            }
        }

        return $config;
    }

    /**
     * Reset PHPUnit's singletons that would otherwise leak state between
     * worker requests. The most dangerous is PassedTests, which retains
     * @depends-satisfying entries forever.
     */
    public static function resetState(): void
    {
        // PassedTests::$instance is private static — reach in via reflection.
        $ref = new \ReflectionClass(PassedTests::class);
        if ($ref->hasProperty('instance')) {
            $prop = $ref->getProperty('instance');
            $prop->setAccessible(true);
            $prop->setValue(null, null);
        }
    }
}
```

- [ ] **Step 2: Smoke-test syntax**

```bash
cd /home/gumiranda/PHPUnit_rust/php && php -l src/Bootstrap.php
```

Expected: `No syntax errors detected`.

- [ ] **Step 3: Smoke-test class resolution**

Write a temporary test to make sure the class loads and the PHPUnit imports resolve:

```bash
cd /home/gumiranda/PHPUnit_rust/php && php -r 'require __DIR__."/vendor/autoload.php"; class_exists(\PhpunitRust\Bootstrap::class) ? print("OK\n") : print("MISSING\n");'
```

Expected: `OK`.

- [ ] **Step 4: Commit**

```bash
cd /home/gumiranda/PHPUnit_rust && git add php/src/Bootstrap.php
git commit -m "feat(worker): add Bootstrap helper for config + state reset"
```

---

## Task 5: Rewrite worker.php to drive TestRunner

This replaces the direct-invocation worker with one that builds a PHPUnit `TestSuite` and runs it via `TestRunner`. Outcomes come from the `ResultCollector` subscriber registered once at boot.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/php/worker.php`

- [ ] **Step 1: Replace `worker.php`**

Replace the contents of `/home/gumiranda/PHPUnit_rust/php/worker.php` with:

```php
<?php

declare(strict_types=1);

require_once __DIR__ . '/vendor/autoload.php';

use PHPUnit\Event\Facade;
use PHPUnit\Framework\TestSuite;
use PHPUnit\Runner\ResultCache\NullResultCache;
use PHPUnit\TextUI\TestRunner;
use PhpunitRust\Bootstrap;
use PhpunitRust\ResultCollector;

ignore_user_abort(true);

// Register the collector ONCE at worker boot. We deliberately never call
// Facade::seal() — sealing would prevent any further registration.
$collector = new ResultCollector();
Facade::instance()->registerSubscribers(
    $collector,                  // notifies fan out by event type inside notify()
);

$loadedAutoloads = [];

$handler = static function () use ($collector, &$loadedAutoloads): void {
    $raw = file_get_contents('php://input');
    $req = json_decode($raw, true);

    header('Content-Type: application/json');

    foreach (['autoload', 'file', 'class', 'methods'] as $required) {
        if (!is_array($req) || !array_key_exists($required, $req)) {
            http_response_code(400);
            echo json_encode(['error' => "missing field: {$required}"]);
            return;
        }
    }
    $autoload = (string) $req['autoload'];
    $file = (string) $req['file'];
    $class = (string) $req['class'];
    $methods = (array) $req['methods'];
    $phpunitXml = isset($req['phpunit_xml']) ? (string) $req['phpunit_xml'] : null;

    if (!is_file($autoload)) {
        http_response_code(400);
        echo json_encode(['error' => "autoload not found: {$autoload}"]);
        return;
    }
    if (!isset($loadedAutoloads[$autoload])) {
        require_once $autoload;
        $loadedAutoloads[$autoload] = true;
    }
    if (!is_file($file)) {
        http_response_code(400);
        echo json_encode(['error' => "test file not found: {$file}"]);
        return;
    }
    require_once $file;
    if (!class_exists($class)) {
        http_response_code(404);
        echo json_encode(['error' => "class {$class} not found after loading {$file}"]);
        return;
    }

    try {
        Bootstrap::configure($phpunitXml);
        Bootstrap::resetState();
        $collector->reset();

        $suite = TestSuite::fromClassName($class);

        // Filter to requested methods if a non-empty list was provided.
        // PHPUnit's TestSuite doesn't expose a clean method-set filter, so
        // we walk the tests it produced and drop the non-matching ones in
        // place via reflection on the private $tests property.
        if (!empty($methods)) {
            self_filter_suite_to_methods($suite, $methods);
        }

        (new TestRunner)->run(\PHPUnit\TextUI\Configuration\Registry::get(), new NullResultCache, $suite);
    } catch (\Throwable $e) {
        http_response_code(500);
        echo json_encode([
            'error'   => 'worker exception while running suite',
            'class'   => $class,
            'detail'  => $e->getMessage(),
            'trace'   => $e->getTraceAsString(),
        ]);
        return;
    }

    echo json_encode(['outcomes' => $collector->outcomes()]);
};

/**
 * Restrict a TestSuite to only those tests whose name matches one of the
 * requested method names (data-provider rows are kept if their base method
 * is in the list).
 */
function self_filter_suite_to_methods(TestSuite $suite, array $methodNames): void
{
    $keep = array_flip($methodNames);
    $ref = new \ReflectionClass($suite);
    $tests = $ref->getProperty('tests');
    $tests->setAccessible(true);
    $current = $tests->getValue($suite);
    $filtered = [];
    foreach ($current as $test) {
        // $test is a TestCase or another TestSuite (for data providers).
        if ($test instanceof \PHPUnit\Framework\TestCase) {
            if (isset($keep[$test->name()])) {
                $filtered[] = $test;
            }
            continue;
        }
        if ($test instanceof TestSuite) {
            // Data-provider wrapper: name is "ClassName::methodName" — keep
            // the whole sub-suite if its method base is in keep.
            $name = $test->name();  // PHPUnit 10 API — getName() doesn't exist on TestSuite
            $baseMethod = strpos($name, '::') !== false
                ? substr($name, strrpos($name, '::') + 2)
                : $name;
            if (isset($keep[$baseMethod])) {
                $filtered[] = $test;
            }
            continue;
        }
    }
    $tests->setValue($suite, $filtered);
}

for ($n = 0; $n < 10000; ++$n) {
    $keep = \frankenphp_handle_request($handler);
    gc_collect_cycles();
    if (!$keep) {
        break;
    }
}
```

**Design notes:**
- `Bootstrap::configure()` runs per request because the worker may receive requests for different projects (different phpunit.xml). It's idempotent per `Registry::init()` — the registry replaces its content cleanly.
- `Bootstrap::resetState()` and `$collector->reset()` happen per request to scrub state from the previous run.
- We never call `Facade::seal()`.
- The `self_filter_suite_to_methods` helper uses reflection on `TestSuite::$tests` because PHPUnit 10's public `Runner\Filter` API requires constructing a `FilterIterator` chain, which is verbose for what we need. This is a deliberate MVP-of-v0.2 trade-off; if the reflection becomes fragile across PHPUnit versions, swap to `\PHPUnit\Runner\Filter\Factory + NameFilterIterator`.

- [ ] **Step 2: Lint the new worker**

```bash
cd /home/gumiranda/PHPUnit_rust/php && php -l worker.php
```

Expected: `No syntax errors detected`.

- [ ] **Step 3: Smoke-test the new worker against the fixture**

```bash
cd /home/gumiranda/PHPUnit_rust/php
pkill -9 -f frankenphp 2>/dev/null; sleep 1
frankenphp php-server --listen 127.0.0.1:8765 --root . --worker $(pwd)/worker.php > /tmp/frankenphp.log 2>&1 &
FPID=$!
sleep 2
php <<'PHP'
<?php
$root = "/home/gumiranda/PHPUnit_rust/fixtures/sample_project";
$ctx = stream_context_create(["http" => [
    "method" => "POST",
    "header" => "Content-Type: application/json\r\n",
    "content" => json_encode([
        "autoload" => "$root/vendor/autoload.php",
        "file" => "$root/tests/CalculatorTest.php",
        "class" => "Sample\\Tests\\CalculatorTest",
        "methods" => [],
    ]),
    "ignore_errors" => true,
    "timeout" => 10,
]]);
$resp = file_get_contents("http://127.0.0.1:8765/worker.php", false, $ctx);
echo "RESPONSE: $resp\n";
PHP
kill $FPID 2>/dev/null
wait $FPID 2>/dev/null
echo "---log tail---"
tail -10 /tmp/frankenphp.log
```

Expected: JSON response with an `outcomes` array of length 3, all `status: "pass"`. In particular, **`testDivisionByZeroThrows` should now be `pass`, not `error`** — proving that PHPUnit's real runner handles `expectException` correctly.

If you see a 500 with "worker exception", read the message and stack trace in the JSON body for diagnosis.

- [ ] **Step 4: Commit**

```bash
cd /home/gumiranda/PHPUnit_rust && git add php/worker.php
git commit -m "feat(worker): drive PHPUnit TestRunner via per-class suites + event subscribers"
```

---

## Task 6: Update Rust HTTP client

The client must now POST a `TestRunRequest` and parse `{outcomes: Vec<TestOutcome>}`.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/client.rs`

- [ ] **Step 1: Replace `src/client.rs`**

```rust
use crate::types::{TestOutcome, TestRunRequest};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
struct WorkerResponse {
    outcomes: Vec<TestOutcome>,
}

pub struct WorkerClient {
    url: String,
    agent: ureq::Agent,
}

impl WorkerClient {
    pub fn new(url: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(60))
            .build();
        Self { url: url.into(), agent }
    }

    pub fn run_class(&self, req: &TestRunRequest) -> Result<Vec<TestOutcome>> {
        let resp = self.agent
            .post(&self.url)
            .set("Content-Type", "application/json")
            .send_json(req)
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => {
                    let body = r.into_string().unwrap_or_default();
                    anyhow!("worker returned HTTP {code}: {body}")
                }
                ureq::Error::Transport(t) => anyhow!("transport error talking to worker: {t}"),
            })?;
        let body: WorkerResponse = resp.into_json().context("worker response was not valid JSON")?;
        Ok(body.outcomes)
    }
}
```

Note the timeout bumped to 60s (whole-class runs can be slower than single-method runs).

- [ ] **Step 2: Build to verify the client compiles**

```bash
cd /home/gumiranda/PHPUnit_rust && cargo build 2>&1 | grep -E "^error" | head -20
```

Expected: errors only in `runner.rs`, `main.rs`, and `tests/integration.rs` (which still use the old API). client.rs itself should compile clean.

- [ ] **Step 3: Commit**

```bash
git add src/client.rs
git commit -m "feat(client): run_class returns Vec<TestOutcome> from {outcomes:[…]} body"
```

---

## Task 7: Update runner.rs to dispatch by class

The runner used to send one request per method; now it groups by class and sends one request per class, collecting all outcomes.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/runner.rs`

- [ ] **Step 1: Replace `src/runner.rs`**

```rust
use crate::client::WorkerClient;
use crate::discovery::{group_by_class, TestClass};
use crate::types::{TestCase, TestOutcome, TestRunRequest, TestStatus};
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub autoload: PathBuf,
    pub phpunit_xml: Option<PathBuf>,
    pub filter: Option<String>,
}

#[derive(Debug)]
pub struct Report {
    pub outcomes: Vec<TestOutcome>,
    pub total_duration_ms: f64,
}

impl Report {
    pub fn count(&self, status: TestStatus) -> usize {
        self.outcomes.iter().filter(|o| o.status == status).count()
    }
    pub fn passed(&self) -> usize { self.count(TestStatus::Pass) }
    pub fn failed(&self) -> usize { self.count(TestStatus::Fail) }
    pub fn errored(&self) -> usize { self.count(TestStatus::Error) }
    pub fn skipped(&self) -> usize { self.count(TestStatus::Skipped) }
    pub fn incomplete(&self) -> usize { self.count(TestStatus::Incomplete) }
    pub fn risky(&self) -> usize { self.count(TestStatus::Risky) }

    pub fn is_success(&self) -> bool {
        self.failed() == 0 && self.errored() == 0
    }
}

pub fn run(
    client: &WorkerClient,
    cases: Vec<TestCase>,
    cfg: &RunConfig,
    mut on_progress: impl FnMut(&TestOutcome),
) -> Result<Report> {
    // Apply class-level filter pre-batch (so we don't ship classes that have
    // no matching methods). Inside a class, the worker filters by methods.
    let filtered_cases: Vec<TestCase> = cases
        .into_iter()
        .filter(|c| match &cfg.filter {
            Some(f) => format!("{}::{}", c.class, c.method).contains(f),
            None => true,
        })
        .collect();

    let groups = group_by_class(filtered_cases);

    let mut outcomes = Vec::new();
    let mut total = 0.0;
    for TestClass { file, class, methods } in groups {
        let req = TestRunRequest {
            autoload: cfg.autoload.clone(),
            phpunit_xml: cfg.phpunit_xml.clone(),
            file,
            class,
            methods,
        };
        let batch = client.run_class(&req)?;
        for outcome in batch {
            total += outcome.duration_ms;
            on_progress(&outcome);
            outcomes.push(outcome);
        }
    }
    Ok(Report { outcomes, total_duration_ms: total })
}
```

- [ ] **Step 2: Build**

```bash
cd /home/gumiranda/PHPUnit_rust && cargo build 2>&1 | grep -E "^error" | head -20
```

Expected: remaining errors are now only in `main.rs` and `tests/integration.rs`.

- [ ] **Step 3: Commit**

```bash
git add src/runner.rs
git commit -m "feat(runner): dispatch by class; Report exposes all 6 status counts"
```

---

## Task 8: Update reporter.rs for Skipped/Incomplete/Risky + dataset

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/reporter.rs`

- [ ] **Step 1: Replace `src/reporter.rs`**

```rust
use crate::runner::Report;
use crate::types::{TestOutcome, TestStatus};
use colored::Colorize;

pub fn print_progress(outcome: &TestOutcome) {
    let mark = match outcome.status {
        TestStatus::Pass => ".".green(),
        TestStatus::Fail => "F".red(),
        TestStatus::Error => "E".yellow(),
        TestStatus::Skipped => "S".cyan(),
        TestStatus::Incomplete => "I".blue(),
        TestStatus::Risky => "R".magenta(),
    };
    print!("{mark}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

pub fn print_summary(report: &Report) {
    println!();
    println!();
    for outcome in &report.outcomes {
        let (label, color) = match outcome.status {
            TestStatus::Pass => continue,
            TestStatus::Fail => ("FAIL", "red"),
            TestStatus::Error => ("ERROR", "yellow"),
            TestStatus::Skipped => ("SKIP", "cyan"),
            TestStatus::Incomplete => ("INCOMPLETE", "blue"),
            TestStatus::Risky => ("RISKY", "magenta"),
        };
        let colored_label = match color {
            "red" => label.red().bold(),
            "yellow" => label.yellow().bold(),
            "cyan" => label.cyan().bold(),
            "blue" => label.blue().bold(),
            "magenta" => label.magenta().bold(),
            _ => label.normal(),
        };
        let name = match &outcome.dataset {
            Some(ds) => format!("{}::{} ({})", outcome.class, outcome.method, ds),
            None => format!("{}::{}", outcome.class, outcome.method),
        };
        println!("{colored_label}  {name}");
        if let Some(msg) = &outcome.message {
            for line in msg.lines() {
                println!("    {line}");
            }
        }
        if let Some(trace) = &outcome.trace {
            for line in trace.lines().take(5) {
                println!("    {}", line.dimmed());
            }
        }
        println!();
    }

    let total = report.outcomes.len();
    let summary = format!(
        "Tests: {total} total, {} passed, {} failed, {} errored, {} skipped, {} incomplete, {} risky ({:.1}ms)",
        report.passed(),
        report.failed(),
        report.errored(),
        report.skipped(),
        report.incomplete(),
        report.risky(),
        report.total_duration_ms,
    );
    if report.is_success() {
        println!("{}", summary.green().bold());
    } else {
        println!("{}", summary.red().bold());
    }
}
```

- [ ] **Step 2: Build**

```bash
cd /home/gumiranda/PHPUnit_rust && cargo build 2>&1 | grep -E "^error" | head -20
```

Expected: only `main.rs` and `tests/integration.rs` still have errors (they don't know about the new types yet).

- [ ] **Step 3: Commit**

```bash
git add src/reporter.rs
git commit -m "feat(reporter): render Skipped/Incomplete/Risky and data-provider datasets"
```

---

## Task 9: Update CLI for phpunit.xml + new dispatch

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/main.rs`

- [ ] **Step 1: Replace `src/main.rs`**

```rust
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

use phpunit_rust::client::WorkerClient;
use phpunit_rust::discovery::discover_in_dir;
use phpunit_rust::frankenphp::{find_worker_script, FrankenPhp};
use phpunit_rust::reporter::{print_progress, print_summary};
use phpunit_rust::runner::{run, RunConfig};

#[derive(Parser, Debug)]
#[command(name = "phpunit-rust", version, about = "PHPUnit-compatible test runner via FrankenPHP")]
struct Cli {
    /// Path to the project under test (must contain composer.json + vendor/).
    #[arg(long, default_value = ".")]
    project: PathBuf,

    /// Subdirectory (relative to --project) containing test files.
    #[arg(long, default_value = "tests")]
    tests_dir: PathBuf,

    /// Run only tests whose `Class::method` contains this substring.
    #[arg(long)]
    filter: Option<String>,

    /// Path to phpunit.xml. Defaults to <project>/phpunit.xml if it exists.
    #[arg(long)]
    configuration: Option<PathBuf>,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn real_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let project = cli.project.canonicalize()
        .with_context(|| format!("project path invalid: {}", cli.project.display()))?;
    let autoload = project.join("vendor/autoload.php");
    if !autoload.is_file() {
        return Err(anyhow!(
            "autoload not found at {}; run `composer install` first",
            autoload.display()
        ));
    }
    let tests_dir = project.join(&cli.tests_dir);
    if !tests_dir.is_dir() {
        return Err(anyhow!("tests directory not found: {}", tests_dir.display()));
    }

    // Auto-detect phpunit.xml if --configuration wasn't supplied.
    let phpunit_xml = match cli.configuration {
        Some(p) => {
            let abs = if p.is_absolute() { p } else { project.join(p) };
            Some(abs.canonicalize().context("invalid --configuration path")?)
        }
        None => {
            let auto = project.join("phpunit.xml");
            if auto.is_file() {
                Some(auto)
            } else {
                let dist = project.join("phpunit.xml.dist");
                if dist.is_file() { Some(dist) } else { None }
            }
        }
    };
    if let Some(p) = &phpunit_xml {
        eprintln!("Using configuration: {}", p.display());
    }

    eprintln!("Discovering tests in {}...", tests_dir.display());
    let cases = discover_in_dir(&tests_dir)?;
    eprintln!("Found {} test methods across {} classes.",
        cases.len(),
        cases.iter().map(|c| &c.class).collect::<std::collections::BTreeSet<_>>().len()
    );

    let worker = find_worker_script()?;
    let fph = FrankenPhp::spawn(&worker)?;
    let client = WorkerClient::new(fph.worker_url());

    let cfg = RunConfig { autoload, phpunit_xml, filter: cli.filter };
    let report = run(&client, cases, &cfg, |o| print_progress(o))?;
    print_summary(&report);

    if report.is_success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}
```

- [ ] **Step 2: Build**

```bash
cd /home/gumiranda/PHPUnit_rust && cargo build 2>&1 | tail -10
```

Expected: only `tests/integration.rs` has remaining errors. The library + binary compile.

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat(cli): add --configuration; auto-detect phpunit.xml; report class counts"
```

---

## Task 10: Update integration tests for new wire

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/tests/integration.rs`

- [ ] **Step 1: Replace `tests/integration.rs`**

```rust
use phpunit_rust::client::WorkerClient;
use phpunit_rust::frankenphp::{find_worker_script, FrankenPhp};
use phpunit_rust::types::{TestRunRequest, TestStatus};
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project")
}

fn request(file: &str, class: &str) -> TestRunRequest {
    let root = fixture_root();
    TestRunRequest {
        autoload: root.join("vendor/autoload.php"),
        phpunit_xml: None,
        file: root.join(file),
        class: class.into(),
        methods: vec![],
    }
}

#[test]
fn calculator_class_all_three_methods_pass() {
    let worker = find_worker_script().expect("worker.php must exist");
    let fph = FrankenPhp::spawn(&worker).expect("frankenphp must spawn");
    let client = WorkerClient::new(fph.worker_url());

    let req = request("tests/CalculatorTest.php", "Sample\\Tests\\CalculatorTest");
    let outcomes = client.run_class(&req).expect("worker call must succeed");

    // Including testDivisionByZeroThrows — now passes because PHPUnit's
    // real runner handles expectException correctly.
    assert_eq!(outcomes.len(), 3, "outcomes: {outcomes:?}");
    for o in &outcomes {
        assert_eq!(o.status, TestStatus::Pass, "{}::{} was {:?}: {:?}", o.class, o.method, o.status, o.message);
    }
}

#[test]
fn failing_class_mixed_results() {
    let worker = find_worker_script().expect("worker.php must exist");
    let fph = FrankenPhp::spawn(&worker).expect("frankenphp must spawn");
    let client = WorkerClient::new(fph.worker_url());

    let req = request("tests/FailingTest.php", "Sample\\Tests\\FailingTest");
    let outcomes = client.run_class(&req).expect("worker call must succeed");

    assert_eq!(outcomes.len(), 2);
    let by_method: std::collections::HashMap<_, _> = outcomes.iter().map(|o| (o.method.clone(), o)).collect();
    assert_eq!(by_method["testThisPasses"].status, TestStatus::Pass);
    assert_eq!(by_method["testThisDeliberatelyFails"].status, TestStatus::Fail);
    assert!(by_method["testThisDeliberatelyFails"].message.as_deref().unwrap_or("").contains("intentional"));
}
```

- [ ] **Step 2: Run all tests**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null; sleep 1
cargo test --lib 2>&1 | tail -5
pkill -9 -f frankenphp 2>/dev/null; sleep 1
cargo test --test integration -- --test-threads=1 2>&1 | tail -10
```

Expected: All lib tests pass; both integration tests pass. Crucially, `calculator_class_all_three_methods_pass` proves `testDivisionByZeroThrows` is now `pass`, not `error`.

- [ ] **Step 3: Commit**

```bash
git add tests/integration.rs
git commit -m "test: integration tests cover class-level dispatch + expectException working"
```

---

## Task 11: Fixture for data providers, @depends, skipped, incomplete

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/src/Repository.php`
- Create: `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/DataProviderTest.php`
- Create: `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/DependsTest.php`
- Create: `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/SkippedTest.php`

- [ ] **Step 1: Subject under test for @depends**

Write `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/src/Repository.php`:

```php
<?php

declare(strict_types=1);

namespace Sample;

final class Repository
{
    /** @var array<int, string> */
    private array $items = [];

    public function add(string $item): int
    {
        $this->items[] = $item;
        return array_key_last($this->items);
    }

    public function get(int $id): string
    {
        return $this->items[$id] ?? throw new \OutOfBoundsException("no item with id {$id}");
    }

    public function count(): int
    {
        return count($this->items);
    }
}
```

- [ ] **Step 2: Data provider test**

Write `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/DataProviderTest.php`:

```php
<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\TestCase;
use Sample\Calculator;

final class DataProviderTest extends TestCase
{
    public static function additionCases(): array
    {
        return [
            'zeros'    => [0, 0, 0],
            'positive' => [2, 3, 5],
            'negative' => [-1, -1, -2],
            'mixed'    => [10, -3, 7],
        ];
    }

    #[DataProvider('additionCases')]
    public function testAddProducesExpectedSum(int $a, int $b, int $expected): void
    {
        $this->assertSame($expected, (new Calculator())->add($a, $b));
    }
}
```

- [ ] **Step 3: @depends test**

Write `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/DependsTest.php`:

```php
<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\Attributes\Depends;
use PHPUnit\Framework\TestCase;
use Sample\Repository;

final class DependsTest extends TestCase
{
    public function testCreatesEmptyRepository(): Repository
    {
        $repo = new Repository();
        $this->assertSame(0, $repo->count());
        return $repo;
    }

    #[Depends('testCreatesEmptyRepository')]
    public function testCanAddItem(Repository $repo): Repository
    {
        $id = $repo->add('first');
        $this->assertSame(0, $id);
        $this->assertSame(1, $repo->count());
        return $repo;
    }

    #[Depends('testCanAddItem')]
    public function testRetainsItemAfterAdd(Repository $repo): void
    {
        $this->assertSame('first', $repo->get(0));
    }
}
```

- [ ] **Step 4: Skipped + incomplete test**

Write `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/SkippedTest.php`:

```php
<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\TestCase;

final class SkippedTest extends TestCase
{
    public function testThatIsExplicitlySkipped(): void
    {
        $this->markTestSkipped('intentionally skipped for runner testing');
    }

    public function testThatIsIncomplete(): void
    {
        $this->markTestIncomplete('not yet finished — intentional for runner testing');
    }

    public function testThatPasses(): void
    {
        $this->assertTrue(true);
    }
}
```

- [ ] **Step 5: Verify vanilla PHPUnit understands the new fixtures**

```bash
cd /home/gumiranda/PHPUnit_rust/fixtures/sample_project && ./vendor/bin/phpunit tests 2>&1 | tail -10
```

Expected output (approximate):

```
OK, but there were issues!
Tests: 13, Assertions: 12, Skipped: 1, Incomplete: 1.
```

Where 13 = the 5 existing tests + 4 data provider rows + 3 depends tests + 3 skipped-class tests = 15. Wait, recount:
- CalculatorTest: 3 tests
- FailingTest: 2 tests (1 still deliberately fails)
- DataProviderTest: 4 data rows
- DependsTest: 3 tests
- SkippedTest: 3 tests (1 skipped, 1 incomplete, 1 passes)

= 15 tests total. With 1 deliberate failure, 1 skipped, 1 incomplete. The exact wording PHPUnit emits varies by version; trust the count + the categories.

- [ ] **Step 6: Commit**

```bash
cd /home/gumiranda/PHPUnit_rust && git add fixtures/sample_project/src/Repository.php fixtures/sample_project/tests/DataProviderTest.php fixtures/sample_project/tests/DependsTest.php fixtures/sample_project/tests/SkippedTest.php
git commit -m "test: fixtures for data providers, @depends, skipped, incomplete"
```

---

## Task 12: End-to-end CLI run validation

**Files:** none modified. This is a verification + acceptance step.

- [ ] **Step 1: Rebuild release**

```bash
cd /home/gumiranda/PHPUnit_rust && cargo build --release 2>&1 | tail -3
```

Expected: clean release build.

- [ ] **Step 2: Run against the expanded fixture**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null; sleep 1
./target/release/phpunit-rust --project fixtures/sample_project 2>&1
echo "exit=$?"
```

Expected output (final summary line, exact counts):

```
Tests: 15 total, 11 passed, 1 failed, 0 errored, 1 skipped, 1 incomplete, 0 risky (<...>ms)
exit=1
```

**The "errored" count must be 0** (previously 1 because of the `expectException` limitation; now fixed). The "failed" count remains 1 (the deliberate failure in FailingTest).

If you see different counts, debug:
- `errored != 0`: TestRunner integration is incomplete; check that `Bootstrap::configure` ran and `TestSuite::fromClassName` returned a populated suite.
- `passed != 11`: data providers may not have expanded (only 1 outcome from DataProviderTest instead of 4), or @depends value-passing failed.
- `skipped != 1`: SkippedTest didn't register the Skipped event.

- [ ] **Step 3: Run filtered**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null; sleep 1
./target/release/phpunit-rust --project fixtures/sample_project --filter DataProvider 2>&1
echo "exit=$?"
```

Expected: only the 4 DataProviderTest rows run, all pass, exit=0.

- [ ] **Step 4: Confirm phpunit.xml auto-detection (sanity)**

The fixture has no phpunit.xml today. Create a trivial one and re-run to confirm it's picked up:

```bash
cd /home/gumiranda/PHPUnit_rust/fixtures/sample_project && cat > phpunit.xml <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<phpunit xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
         bootstrap="vendor/autoload.php"
         colors="true">
    <testsuites>
        <testsuite name="default">
            <directory>tests</directory>
        </testsuite>
    </testsuites>
</phpunit>
XML
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null; sleep 1
./target/release/phpunit-rust --project fixtures/sample_project 2>&1 | head -5
```

Expected first lines:

```
Using configuration: /home/gumiranda/PHPUnit_rust/fixtures/sample_project/phpunit.xml
Discovering tests in ...
```

Then clean up the test file (don't commit it):

```bash
rm /home/gumiranda/PHPUnit_rust/fixtures/sample_project/phpunit.xml
```

- [ ] **Step 5: Run the integration test suite**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null; sleep 1
cargo test --test integration -- --test-threads=1 2>&1 | tail -5
```

Expected: 2 passed.

- [ ] **Step 6: No commit for this task**

This is a verification gate, not new code. Move on to Task 13.

---

## Task 13: Update README

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/README.md`

- [ ] **Step 1: Replace README.md**

Write `/home/gumiranda/PHPUnit_rust/README.md`:

```markdown
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

```
phpunit-rust (Rust binary)
  ├─ discovery   : tree-sitter-php parses test files, groups by class
  ├─ frankenphp  : spawns FrankenPHP child in worker mode; HTTP readiness probe
  ├─ client      : POSTs TestRunRequest, parses {outcomes:[…]}
  ├─ runner      : one HTTP request per test class
  └─ reporter    : TTY output (Pass/Fail/Error/Skip/Incomplete/Risky)

worker.php (long-lived in FrankenPHP worker mode)
  ├─ Bootstrap::configure(phpunit.xml?)  → Registry::init + bootstrap require
  ├─ Bootstrap::resetState()             → null PassedTests singleton
  ├─ ResultCollector->reset()
  ├─ TestSuite::fromClassName(class)     → filter by methods if requested
  ├─ TestRunner::run(...)                → fires Test\Passed/Failed/etc events
  └─ collector->outcomes()               → returned as JSON {outcomes:[…]}

ResultCollector (subscriber registered once with Facade::instance())
  └─ implements PassedSubscriber, FailedSubscriber, ErroredSubscriber,
     SkippedSubscriber, MarkedIncompleteSubscriber, ConsideredRiskySubscriber,
     PreparationStartedSubscriber, FinishedSubscriber
```

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
    {"class":"...","method":"testBar","dataset":null,"status":"pass","message":null,"trace":null,"duration_ms":1.2},
    {"class":"...","method":"testBaz","dataset":"#0","status":"fail","message":"…","trace":"…","duration_ms":0.3}
  ]
}
```
```

- [ ] **Step 2: Commit**

```bash
cd /home/gumiranda/PHPUnit_rust && git add README.md
git commit -m "docs: README updates for v0.2.0 (real PHPUnit Runner integration)"
```

---

## Self-Review

**Spec coverage (per user's locked-in choices):**
- ✅ `expectException` — Task 5 (real TestRunner) + Task 10 (assertion in integration test) + Task 12 (count assertion).
- ✅ Data providers — Task 11 (DataProviderTest fixture) + Task 12 (counts include 4 rows).
- ✅ Skipped/incomplete/risky — Task 1 (new statuses) + Task 8 (reporter) + Task 11 (SkippedTest fixture) + Task 12 (assertions).
- ✅ `@depends` — Task 4 (PassedTests reset) + Task 11 (DependsTest with 3-step chain) + Task 12 (verified passing).
- ✅ phpunit.xml support — Task 4 (Bootstrap::configure) + Task 9 (CLI auto-detect + --configuration) + Task 12 Step 4 (sanity test).
- ✅ Clean wire break to `Vec<TestOutcome>` — Task 1 (types) + Task 6 (client) + Task 7 (runner).

**Placeholder scan:** No "TBD", no "implement later", no "add error handling." Each task has complete, runnable code. Deferred features (parallel, coverage, custom extensions) are flagged in the README as scope-out, not as TODOs to satisfy here.

**Type consistency:**
- `TestStatus` adds `Skipped`, `Incomplete`, `Risky` (Task 1). `Report` exposes counts for all 6 (Task 7). Reporter renders all 6 (Task 8). Worker emits matching lowercase strings (`'skipped'`, `'incomplete'`, `'risky'`) from `ResultCollector::record()` (Task 3).
- `TestRunRequest` adds `phpunit_xml` + `methods` (Task 1). Worker reads both with the same names (Task 5). Runner constructs with same names (Task 7). CLI populates both (Task 9). Integration test uses same names (Task 10).
- `TestOutcome.dataset` (Task 1). Worker emits `dataset` field (Task 3). Reporter displays it parenthetically (Task 8).

**Pitfalls re-checked:**
- No `Facade::seal()` call anywhere (verified Task 5).
- `PassedTests` reset via reflection per request (Task 4 + called from worker in Task 5).
- `TestSuite::fromClassName()` called fresh per request (Task 5).
- `ResultCollector` registered once at boot, reset per request (Task 5).

## Out-of-scope (deferred to follow-up plans)

- Parallel execution (requires test-isolation strategy first)
- `@runInSeparateProcess` (one-shot worker fallback)
- Code coverage (PCOV/Xdebug, Clover/Cobertura)
- JUnit XML / TAP / TestDox reporters
- Watch mode (notify crate)
- Custom PHPUnit extensions + listeners (rare; defer until requested)
- `@requires` annotations (PHP/extension version skipping)
- Test source-file filter via `phpunit.xml`'s `<testsuites>` directives (we use our own discovery today)
