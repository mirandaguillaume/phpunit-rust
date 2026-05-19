# Own the Runner — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace PHPUnit's `TestRunner` with our own minimal test executor (PHP-side `PhpunitRust\TestExecutor` + supporting helpers), so we depend only on PHPUnit's *stable* user-facing surface (`TestCase`, assertions, mocks, exception markers) and stop tracking PHPUnit's evolving runner internals.

**Architecture:** Worker no longer delegates to PHPUnit. It receives `{class, methods[]}`, instantiates the test class, walks methods in dependency order (resolving `#[Depends]`), expands data providers ourselves (calling provider methods directly), invokes `setUp`/test/`tearDown` via reflection, and classifies the result by catching PHPUnit's marker exceptions (`SkippedWithMessageException`, `IncompleteTestError`, `ExpectationFailedException`) plus generic `Throwable`. `expectException` is honored by reading the TestCase's expectation state via reflection after the test method returns. `setUpBeforeClass`/`tearDownAfterClass` run once per class request. No more `Facade`, no more `Registry`, no more `PassedTests` singleton — those were PHPUnit internals we no longer touch.

**Tech Stack:**
- Existing: Rust 1.75+, FrankenPHP 1.x (PHP 8.5 bundled), `tree-sitter-php`, `clap`, `ureq`, `colored`
- New PHP dependencies: still PHPUnit 10.x **for tests of our PHP code only** (`require-dev`); production worker code only references the user-facing surface
- New Rust dependencies: `quick-xml` (parse `phpunit.xml`'s `bootstrap` attribute)

**Verification notes for the implementer (read before starting):**
- **Don't import PHPUnit internals in `worker.php` or `TestExecutor.php` beyond these:** `PHPUnit\Framework\TestCase`, `PHPUnit\Framework\ExpectationFailedException`, `PHPUnit\Framework\IncompleteTestError`, `PHPUnit\Framework\SkippedWithMessageException`, `PHPUnit\Framework\AssertionFailedError`, `PHPUnit\Framework\Attributes\DataProvider`, `PHPUnit\Framework\Attributes\Depends`. These have been stable across PHPUnit 9, 10, 11, 12.
- **PHPUnit's `markTestSkipped()` throws `SkippedWithMessageException` in PHPUnit 10+** (was `SkippedTestError` in 9 and earlier). If we need to support 9, branch on `class_exists()`. For now assume ≥10.
- **`expectException()` records the expected class in private TestCase properties.** We read it back via reflection after the test method returns. If the property layout changes across versions, this needs a fallback — use `method_exists()` on `TestCase` to find a stable accessor.

---

## File Structure

```
src/main.rs                         # add --bootstrap CLI flag; parse phpunit.xml bootstrap
src/runner.rs                       # unchanged structurally; same wire shape
src/discovery.rs                    # unchanged
src/client.rs                       # unchanged
src/frankenphp.rs                   # unchanged
src/types.rs                        # unchanged
src/phpunit_xml.rs                  # NEW: minimal parser for <phpunit bootstrap="..."> attr
php/composer.json                   # demote PHPUnit to require-dev (for our own tests only)
php/src/TestExecutor.php            # NEW: the heart of the pivot — runs one class's tests
php/src/MethodPlanner.php           # NEW: discovers data providers + @depends; orders methods
php/src/OutcomeBuilder.php          # NEW: classifies exceptions into pass/fail/error/skipped/incomplete
php/src/Bootstrap.php               # REMOVE: no more Registry/PhpHandler/PassedTests reset
php/src/ResultCollector.php         # REMOVE: no more Facade subscribers
php/src/ResultCollector*Subscriber.php  # REMOVE: same
php/worker.php                      # REWRITE: thin shell that delegates to TestExecutor
php/tests/TestExecutorTest.php      # NEW: PHPUnit tests for our TestExecutor
php/tests/MethodPlannerTest.php     # NEW: tests for the dependency/dataprovider planner
php/tests/OutcomeBuilderTest.php    # NEW: tests for exception classification
fixtures/sample_project/...         # unchanged — must still pass after pivot
docs/superpowers/plans/2026-05-19-own-the-runner.md  # this plan
```

**Boundaries:**
- `TestExecutor` is the only class that touches the test instance. It instantiates, invokes lifecycle, captures outcomes.
- `MethodPlanner` is pure: takes a class name + a list of methods, returns a sequence of `(method, dataset, depends_args_ref)` tuples in dependency order.
- `OutcomeBuilder` is pure: given a status enum and optional exception, returns the JSON-ready outcome dict.
- `worker.php` is now ~50 lines: receive request → call `TestExecutor::runClass(...)` → return outcomes JSON.

---

## Task 1: Add quick-xml dep + phpunit.xml bootstrap parser

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/Cargo.toml`
- Create: `/home/gumiranda/PHPUnit_rust/src/phpunit_xml.rs`
- Modify: `/home/gumiranda/PHPUnit_rust/src/lib.rs`

- [ ] **Step 1: Add quick-xml to dependencies**

In `Cargo.toml`, under `[dependencies]`, add:

```toml
quick-xml = "0.36"
```

- [ ] **Step 2: Write the failing test**

Append to `src/phpunit_xml.rs` (file will be created in step 3, but plan the test first):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bootstrap_attribute_from_phpunit_xml() {
        let xml = r#"<?xml version="1.0"?>
<phpunit bootstrap="phpunit.php" colors="true">
    <testsuites>
        <testsuite name="default">
            <directory>tests</directory>
        </testsuite>
    </testsuites>
</phpunit>"#;
        let bootstrap = parse_bootstrap(xml);
        assert_eq!(bootstrap.as_deref(), Some("phpunit.php"));
    }

    #[test]
    fn returns_none_when_no_bootstrap_attribute() {
        let xml = r#"<?xml version="1.0"?>
<phpunit colors="true"></phpunit>"#;
        assert!(parse_bootstrap(xml).is_none());
    }

    #[test]
    fn returns_none_on_malformed_xml() {
        assert!(parse_bootstrap("this is not xml").is_none());
    }
}
```

- [ ] **Step 3: Create `src/phpunit_xml.rs`**

```rust
//! Minimal parser for the bits of `phpunit.xml` we honor in our runner.
//! We deliberately do NOT parse `<testsuites>`, `<source>`, `<extensions>`,
//! etc. — our own discovery handles test enumeration. The only attribute
//! we currently care about is `bootstrap` on the root `<phpunit>` element.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

/// Returns the value of the `bootstrap` attribute on the root `<phpunit>`
/// element, or None if absent / file is malformed.
pub fn parse_bootstrap(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == b"phpunit" {
                    for attr in e.attributes().flatten() {
                        if attr.key.local_name().as_ref() == b"bootstrap" {
                            return std::str::from_utf8(&attr.value).ok().map(String::from);
                        }
                    }
                    return None;
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}
```

- [ ] **Step 4: Register in lib.rs**

Append to `src/lib.rs`:

```rust
pub mod phpunit_xml;
```

(Insert in alphabetical order in the existing `pub mod` block.)

- [ ] **Step 5: Run the tests**

```bash
cd /home/gumiranda/PHPUnit_rust && cargo test --lib phpunit_xml
```

Expected: `test result: ok. 3 passed; 0 failed`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/phpunit_xml.rs src/lib.rs
git commit -m "feat(phpunit-xml): minimal parser for bootstrap attribute"
```

---

## Task 2: PHP `OutcomeBuilder` — exception classification

This isolates the pass/fail/error/skipped/incomplete decision logic into one tested class. `TestExecutor` (later tasks) calls it.

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/php/src/OutcomeBuilder.php`
- Create: `/home/gumiranda/PHPUnit_rust/php/tests/OutcomeBuilderTest.php`
- Modify: `/home/gumiranda/PHPUnit_rust/php/composer.json` (add `autoload-dev` for our tests)

- [ ] **Step 1: Update composer.json**

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
    "autoload-dev": {
        "psr-4": {
            "PhpunitRust\\Tests\\": "tests/"
        }
    },
    "require": {
        "php": ">=8.1"
    },
    "require-dev": {
        "phpunit/phpunit": "^10.5"
    }
}
```

Run from `/home/gumiranda/PHPUnit_rust/php`:

```bash
composer update --no-interaction
```

- [ ] **Step 2: Write the failing test**

`php/tests/OutcomeBuilderTest.php`:

```php
<?php

declare(strict_types=1);

namespace PhpunitRust\Tests;

use PhpunitRust\OutcomeBuilder;
use PHPUnit\Framework\AssertionFailedError;
use PHPUnit\Framework\ExpectationFailedException;
use PHPUnit\Framework\IncompleteTestError;
use PHPUnit\Framework\SkippedWithMessageException;
use PHPUnit\Framework\TestCase;

final class OutcomeBuilderTest extends TestCase
{
    public function testPassNoException(): void
    {
        $outcome = OutcomeBuilder::build('A', 'm', null, 1.5, null);
        $this->assertSame('pass', $outcome['status']);
        $this->assertNull($outcome['message']);
        $this->assertSame(1.5, $outcome['duration_ms']);
    }

    public function testFailFromExpectationFailed(): void
    {
        $e = new ExpectationFailedException('boom');
        $outcome = OutcomeBuilder::build('A', 'm', null, 0.1, $e);
        $this->assertSame('fail', $outcome['status']);
        $this->assertSame('boom', $outcome['message']);
        $this->assertNotNull($outcome['trace']);
    }

    public function testFailFromAssertionFailed(): void
    {
        $e = new AssertionFailedError('nope');
        $outcome = OutcomeBuilder::build('A', 'm', null, 0.1, $e);
        $this->assertSame('fail', $outcome['status']);
    }

    public function testSkipped(): void
    {
        $e = new SkippedWithMessageException('because reasons');
        $outcome = OutcomeBuilder::build('A', 'm', null, 0.1, $e);
        $this->assertSame('skipped', $outcome['status']);
        $this->assertSame('because reasons', $outcome['message']);
    }

    public function testIncomplete(): void
    {
        $e = new IncompleteTestError('todo');
        $outcome = OutcomeBuilder::build('A', 'm', null, 0.1, $e);
        $this->assertSame('incomplete', $outcome['status']);
    }

    public function testGenericThrowableBecomesError(): void
    {
        $e = new \RuntimeException('crash');
        $outcome = OutcomeBuilder::build('A', 'm', null, 0.1, $e);
        $this->assertSame('error', $outcome['status']);
        $this->assertStringContainsString('RuntimeException', $outcome['message']);
    }

    public function testDatasetIsPropagated(): void
    {
        $outcome = OutcomeBuilder::build('A', 'm', 'with foo', 0.1, null);
        $this->assertSame('with foo', $outcome['dataset']);
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

```bash
cd /home/gumiranda/PHPUnit_rust/php && ./vendor/bin/phpunit tests/OutcomeBuilderTest.php 2>&1 | tail -5
```

Expected: errors about `PhpunitRust\OutcomeBuilder` not found.

- [ ] **Step 4: Implement `OutcomeBuilder`**

`php/src/OutcomeBuilder.php`:

```php
<?php

declare(strict_types=1);

namespace PhpunitRust;

use PHPUnit\Framework\AssertionFailedError;
use PHPUnit\Framework\ExpectationFailedException;
use PHPUnit\Framework\IncompleteTestError;
use PHPUnit\Framework\SkippedWithMessageException;

/**
 * Classifies a (possibly null) exception thrown during a test run into the
 * canonical phpunit-rust outcome shape. Pure / side-effect-free.
 */
final class OutcomeBuilder
{
    /**
     * @return array{class:string,method:string,dataset:?string,status:string,message:?string,trace:?string,duration_ms:float}
     */
    public static function build(
        string $class,
        string $method,
        ?string $dataset,
        float $durationMs,
        ?\Throwable $error,
    ): array {
        [$status, $message, $trace] = self::classify($error);
        return [
            'class'       => $class,
            'method'      => $method,
            'dataset'     => $dataset,
            'status'      => $status,
            'message'     => $message,
            'trace'       => $trace,
            'duration_ms' => $durationMs,
        ];
    }

    /**
     * @return array{0:string,1:?string,2:?string}
     */
    private static function classify(?\Throwable $error): array
    {
        if ($error === null) {
            return ['pass', null, null];
        }
        if ($error instanceof SkippedWithMessageException) {
            return ['skipped', $error->getMessage(), null];
        }
        if ($error instanceof IncompleteTestError) {
            return ['incomplete', $error->getMessage(), $error->getTraceAsString()];
        }
        if ($error instanceof ExpectationFailedException || $error instanceof AssertionFailedError) {
            return ['fail', $error->getMessage(), $error->getTraceAsString()];
        }
        $msg = get_class($error) . ': ' . $error->getMessage();
        return ['error', $msg, $error->getTraceAsString()];
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd /home/gumiranda/PHPUnit_rust/php && ./vendor/bin/phpunit tests/OutcomeBuilderTest.php 2>&1 | tail -5
```

Expected: `OK (7 tests, ...)`.

- [ ] **Step 6: Commit**

```bash
git add php/composer.json php/composer.lock php/src/OutcomeBuilder.php php/tests/OutcomeBuilderTest.php
git commit -m "feat(php): OutcomeBuilder classifies exceptions into pass/fail/error/skipped/incomplete"
```

---

## Task 3: PHP `MethodPlanner` — data providers + @depends ordering

Single class that turns "run these methods of this test class" into a concrete ordered sequence of `(method, dataset_key, dataset_args)` triples, honoring `#[DataProvider]` expansion and `#[Depends]` ordering.

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/php/src/MethodPlanner.php`
- Create: `/home/gumiranda/PHPUnit_rust/php/tests/MethodPlannerTest.php`

- [ ] **Step 1: Write the failing test**

`php/tests/MethodPlannerTest.php`:

```php
<?php

declare(strict_types=1);

namespace PhpunitRust\Tests;

use PhpunitRust\MethodPlanner;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\Attributes\Depends;
use PHPUnit\Framework\TestCase;

// Sample fixtures inside the test file (single-file scope).
final class _MpSimple extends TestCase
{
    public function testOne(): void {}
    public function testTwo(): void {}
}

final class _MpProvider extends TestCase
{
    public static function rows(): array
    {
        return ['a' => [1, 2], 'b' => [3, 4]];
    }
    #[DataProvider('rows')]
    public function testParam(int $a, int $b): void {}
}

final class _MpChain extends TestCase
{
    public function testRoot(): int { return 1; }
    #[Depends('testRoot')]
    public function testMiddle(int $r): int { return $r + 1; }
    #[Depends('testMiddle')]
    public function testLeaf(int $r): void {}
}

final class MethodPlannerTest extends TestCase
{
    public function testNonProviderMethodEmitsSingleStep(): void
    {
        $steps = MethodPlanner::plan(_MpSimple::class, ['testOne', 'testTwo']);
        $this->assertCount(2, $steps);
        $this->assertSame('testOne', $steps[0]['method']);
        $this->assertNull($steps[0]['dataset']);
        $this->assertSame([], $steps[0]['args']);
    }

    public function testDataProviderExpandsToOneStepPerRow(): void
    {
        $steps = MethodPlanner::plan(_MpProvider::class, ['testParam']);
        $this->assertCount(2, $steps);
        $this->assertSame('a', $steps[0]['dataset']);
        $this->assertSame([1, 2], $steps[0]['args']);
        $this->assertSame('b', $steps[1]['dataset']);
        $this->assertSame([3, 4], $steps[1]['args']);
    }

    public function testDependsOrdersMethodsTopologically(): void
    {
        // Give in reverse order to prove planner re-orders.
        $steps = MethodPlanner::plan(_MpChain::class, ['testLeaf', 'testMiddle', 'testRoot']);
        $names = array_column($steps, 'method');
        $this->assertSame(['testRoot', 'testMiddle', 'testLeaf'], $names);
    }

    public function testDependsAreReturnedInStep(): void
    {
        $steps = MethodPlanner::plan(_MpChain::class, ['testRoot', 'testMiddle']);
        $this->assertSame([], $steps[0]['depends']);
        $this->assertSame(['testRoot'], $steps[1]['depends']);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /home/gumiranda/PHPUnit_rust/php && ./vendor/bin/phpunit tests/MethodPlannerTest.php 2>&1 | tail -5
```

Expected: `PhpunitRust\MethodPlanner` not found.

- [ ] **Step 3: Implement `MethodPlanner`**

`php/src/MethodPlanner.php`:

```php
<?php

declare(strict_types=1);

namespace PhpunitRust;

use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\Attributes\Depends;

/**
 * Turns "run these methods of this class" into an ordered sequence of steps.
 * Each step is one invocation: a method, optional dataset key, optional args
 * from a data provider, and the list of dependencies whose return values
 * should be prepended to args at invocation time.
 *
 * @phpstan-type Step array{method:string,dataset:?string,args:list<mixed>,depends:list<string>}
 */
final class MethodPlanner
{
    /**
     * @param class-string $class
     * @param list<string> $methods Empty = all `test*` methods on the class.
     * @return list<Step>
     */
    public static function plan(string $class, array $methods): array
    {
        $ref = new \ReflectionClass($class);
        $candidate = empty($methods) ? self::allTestMethods($ref) : $methods;

        // Resolve dependencies and topologically sort.
        $ordered = self::topoSort($ref, $candidate);

        $steps = [];
        foreach ($ordered as $methodName) {
            $methodRef = $ref->getMethod($methodName);
            $depends   = self::dependsOf($methodRef);
            $datasets  = self::dataSetsFor($ref, $methodRef);
            if ($datasets === null) {
                // Non-parameterized: one step.
                $steps[] = [
                    'method'  => $methodName,
                    'dataset' => null,
                    'args'    => [],
                    'depends' => $depends,
                ];
                continue;
            }
            foreach ($datasets as $key => $row) {
                $steps[] = [
                    'method'  => $methodName,
                    'dataset' => is_int($key) ? "#{$key}" : (string) $key,
                    'args'    => array_values(is_array($row) ? $row : iterator_to_array($row)),
                    'depends' => $depends,
                ];
            }
        }
        return $steps;
    }

    /** @return list<string> */
    private static function allTestMethods(\ReflectionClass $ref): array
    {
        $out = [];
        foreach ($ref->getMethods(\ReflectionMethod::IS_PUBLIC) as $m) {
            if ($m->getDeclaringClass()->isAbstract()) {
                continue;
            }
            if (str_starts_with($m->getName(), 'test')) {
                $out[] = $m->getName();
            }
        }
        return $out;
    }

    /** @return list<string> */
    private static function dependsOf(\ReflectionMethod $m): array
    {
        $out = [];
        foreach ($m->getAttributes(Depends::class) as $attr) {
            $instance = $attr->newInstance();
            // Depends::methodName() is the public accessor in PHPUnit 10+.
            $out[] = $instance->methodName();
        }
        return $out;
    }

    /** @return iterable<int|string, mixed>|null */
    private static function dataSetsFor(\ReflectionClass $ref, \ReflectionMethod $m): ?iterable
    {
        $providers = $m->getAttributes(DataProvider::class);
        if (empty($providers)) {
            return null;
        }
        $rows = [];
        foreach ($providers as $attr) {
            $providerName = $attr->newInstance()->methodName();
            $providerRef  = $ref->getMethod($providerName);
            $providerRef->setAccessible(true);
            $result = $providerRef->isStatic()
                ? $providerRef->invoke(null)
                : $providerRef->invoke($ref->newInstanceWithoutConstructor());
            foreach ($result as $key => $row) {
                $rows[$key] = $row;
            }
        }
        return $rows;
    }

    /**
     * Topological sort of `$methods` honoring `#[Depends]`. Depended-on
     * methods come first; methods NOT in `$methods` that are depended on
     * are NOT added (callers asked only for `$methods`; we don't auto-pull
     * deps — that's the worker's job, see TestExecutor cache semantics).
     *
     * @param list<string> $methods
     * @return list<string>
     */
    private static function topoSort(\ReflectionClass $ref, array $methods): array
    {
        $set = array_flip($methods);
        $visited = [];
        $out = [];
        $visit = function (string $m) use (&$visit, &$visited, &$out, $ref, $set): void {
            if (isset($visited[$m])) return;
            $visited[$m] = true;
            if (!isset($set[$m])) return;
            foreach (self::dependsOf($ref->getMethod($m)) as $dep) {
                $visit($dep);
            }
            $out[] = $m;
        };
        foreach ($methods as $m) {
            $visit($m);
        }
        return $out;
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cd /home/gumiranda/PHPUnit_rust/php && ./vendor/bin/phpunit tests/MethodPlannerTest.php 2>&1 | tail -5
```

Expected: `OK (4 tests, ...)`.

- [ ] **Step 5: Commit**

```bash
git add php/src/MethodPlanner.php php/tests/MethodPlannerTest.php
git commit -m "feat(php): MethodPlanner expands data providers and orders by @depends"
```

---

## Task 4: PHP `TestExecutor` — the core executor

The class that actually runs a test class. Takes a class + planned steps, returns outcomes.

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/php/src/TestExecutor.php`
- Create: `/home/gumiranda/PHPUnit_rust/php/tests/TestExecutorTest.php`

- [ ] **Step 1: Write the failing test**

`php/tests/TestExecutorTest.php`:

```php
<?php

declare(strict_types=1);

namespace PhpunitRust\Tests;

use PhpunitRust\TestExecutor;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\Attributes\Depends;
use PHPUnit\Framework\TestCase;

final class _ExecPass extends TestCase
{
    public function testYes(): void { $this->assertTrue(true); }
}

final class _ExecFail extends TestCase
{
    public function testNo(): void { $this->assertSame(1, 2); }
}

final class _ExecExpectException extends TestCase
{
    public function testThrows(): void
    {
        $this->expectException(\RuntimeException::class);
        throw new \RuntimeException('expected');
    }
}

final class _ExecSkipped extends TestCase
{
    public function testSkip(): void { $this->markTestSkipped('nope'); }
    public function testIncomplete(): void { $this->markTestIncomplete('wip'); }
}

final class _ExecProvider extends TestCase
{
    public static function rows(): array { return [[1, 1], [2, 4], [3, 9]]; }
    #[DataProvider('rows')]
    public function testSquare(int $in, int $expected): void
    {
        $this->assertSame($expected, $in * $in);
    }
}

final class _ExecChain extends TestCase
{
    public function testRoot(): array { return ['hello']; }
    #[Depends('testRoot')]
    public function testChild(array $v): void
    {
        $this->assertSame(['hello'], $v);
    }
}

final class TestExecutorTest extends TestCase
{
    public function testPassingTestProducesPassOutcome(): void
    {
        $outcomes = TestExecutor::runClass(_ExecPass::class, ['testYes']);
        $this->assertCount(1, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status']);
    }

    public function testFailingAssertionProducesFailOutcome(): void
    {
        $outcomes = TestExecutor::runClass(_ExecFail::class, ['testNo']);
        $this->assertSame('fail', $outcomes[0]['status']);
    }

    public function testExpectExceptionPasses(): void
    {
        $outcomes = TestExecutor::runClass(_ExecExpectException::class, ['testThrows']);
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
    }

    public function testSkippedProducesSkippedOutcome(): void
    {
        $outcomes = TestExecutor::runClass(_ExecSkipped::class, ['testSkip']);
        $this->assertSame('skipped', $outcomes[0]['status']);
        $this->assertSame('nope', $outcomes[0]['message']);
    }

    public function testIncompleteProducesIncompleteOutcome(): void
    {
        $outcomes = TestExecutor::runClass(_ExecSkipped::class, ['testIncomplete']);
        $this->assertSame('incomplete', $outcomes[0]['status']);
    }

    public function testDataProviderExpandsToMultiplePassingOutcomes(): void
    {
        $outcomes = TestExecutor::runClass(_ExecProvider::class, ['testSquare']);
        $this->assertCount(3, $outcomes);
        foreach ($outcomes as $o) {
            $this->assertSame('pass', $o['status'], var_export($o, true));
        }
        $this->assertSame('#0', $outcomes[0]['dataset']);
    }

    public function testDependsInjectsReturnValueAsArg(): void
    {
        $outcomes = TestExecutor::runClass(_ExecChain::class, ['testRoot', 'testChild']);
        $this->assertCount(2, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status']);
        $this->assertSame('pass', $outcomes[1]['status'], var_export($outcomes[1], true));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /home/gumiranda/PHPUnit_rust/php && ./vendor/bin/phpunit tests/TestExecutorTest.php 2>&1 | tail -5
```

Expected: `PhpunitRust\TestExecutor` not found.

- [ ] **Step 3: Implement `TestExecutor`**

`php/src/TestExecutor.php`:

```php
<?php

declare(strict_types=1);

namespace PhpunitRust;

use PHPUnit\Framework\TestCase;

/**
 * Runs every step of a planned class invocation. Handles setUp/tearDown,
 * setUpBeforeClass/tearDownAfterClass, expectException, and dependency
 * return-value injection. No knowledge of PHPUnit's TestRunner, Facade,
 * or Configuration — we only call into the user-facing TestCase API.
 */
final class TestExecutor
{
    /**
     * @param class-string $class
     * @param list<string> $methods Empty = all `test*` methods on the class.
     * @return list<array<string, mixed>>
     */
    public static function runClass(string $class, array $methods): array
    {
        if (!is_subclass_of($class, TestCase::class)) {
            throw new \InvalidArgumentException("$class does not extend PHPUnit\\Framework\\TestCase");
        }

        $steps = MethodPlanner::plan($class, $methods);
        $ref   = new \ReflectionClass($class);

        // setUpBeforeClass (PHPUnit's per-class hook, protected static).
        self::invokeOptionalStatic($ref, 'setUpBeforeClass');

        $outcomes = [];
        $passedReturns = [];  // method -> return value, for @depends injection

        foreach ($steps as $step) {
            $method  = $step['method'];
            $dataset = $step['dataset'];
            $userArgs = $step['args'];
            $depends = $step['depends'];

            // Build args = depends-return-values ++ data-provider-args.
            $args = [];
            $missingDep = null;
            foreach ($depends as $dep) {
                if (!array_key_exists($dep, $passedReturns)) {
                    $missingDep = $dep;
                    break;
                }
                $args[] = $passedReturns[$dep];
            }
            if ($missingDep !== null) {
                $outcomes[] = OutcomeBuilder::build(
                    $class, $method, $dataset, 0.0,
                    new \PHPUnit\Framework\SkippedWithMessageException(
                        "missing dependency: {$missingDep}"
                    )
                );
                continue;
            }
            $args = array_merge($args, $userArgs);

            $startedAt = microtime(true);
            $error = null;
            $returnValue = null;

            try {
                $test = new $class($method);
                self::invokeOptional($test, 'setUp');
                $returnValue = $test->{$method}(...$args);
                self::invokeOptional($test, 'tearDown');

                // expectException check: if the test declared one and didn't
                // throw, that's a failure. PHPUnit's runner verifies this
                // automatically via the runBare flow we're bypassing.
                if ($expected = self::readExpectedException($test)) {
                    $error = new \PHPUnit\Framework\ExpectationFailedException(
                        "Expected exception {$expected} was not thrown"
                    );
                }
            } catch (\PHPUnit\Framework\SkippedWithMessageException $e) {
                $error = $e;
            } catch (\PHPUnit\Framework\IncompleteTestError $e) {
                $error = $e;
            } catch (\Throwable $e) {
                // Was this exception expected via expectException()?
                if (isset($test) && self::wasExceptionExpected($test, $e)) {
                    // Pass. Run tearDown if setUp succeeded — best-effort.
                    self::invokeOptional($test, 'tearDown');
                } else {
                    $error = $e;
                }
            }

            $duration = (microtime(true) - $startedAt) * 1000.0;
            $outcomes[] = OutcomeBuilder::build($class, $method, $dataset, $duration, $error);

            if ($error === null && $returnValue !== null) {
                $passedReturns[$method] = $returnValue;
            }
        }

        self::invokeOptionalStatic($ref, 'tearDownAfterClass');

        return $outcomes;
    }

    private static function invokeOptional(TestCase $test, string $name): void
    {
        // Bind a closure to the test instance's scope so we can call protected
        // setUp / tearDown without ReflectionMethod::setAccessible (deprecated
        // in PHP 8.1+ when applied to non-private members).
        //
        // NOTE: the closure is intentionally non-static. `Closure::bind` cannot
        // attach an instance to a static closure (PHP raises "Cannot bind an
        // instance to a static closure"), so this must NOT be `static function`.
        \Closure::bind(function () use ($name) {
            // @phpstan-ignore-next-line
            $this->{$name}();
        }, $test, $test)();
    }

    private static function invokeOptionalStatic(\ReflectionClass $ref, string $name): void
    {
        if (!$ref->hasMethod($name)) return;
        $m = $ref->getMethod($name);
        if (!$m->isStatic()) return;
        $m->setAccessible(true);
        $m->invoke(null);
    }

    /**
     * Returns the FQCN of the exception class declared via expectException(),
     * or null if none was declared.
     */
    private static function readExpectedException(TestCase $test): ?string
    {
        // PHPUnit stores the expected exception in a private property of
        // TestCase (the parent). ReflectionObject on a subclass does NOT
        // expose inherited private properties, so we reflect on TestCase
        // itself. Property name has been stable across PHPUnit 9, 10, 11.
        $ref = new \ReflectionClass(TestCase::class);
        if (!$ref->hasProperty('expectedException')) return null;
        $prop = $ref->getProperty('expectedException');
        $prop->setAccessible(true);
        $value = $prop->getValue($test);
        return is_string($value) ? $value : null;
    }

    private static function wasExceptionExpected(TestCase $test, \Throwable $thrown): bool
    {
        $expected = self::readExpectedException($test);
        return $expected !== null && ($thrown instanceof $expected);
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cd /home/gumiranda/PHPUnit_rust/php && ./vendor/bin/phpunit tests/TestExecutorTest.php 2>&1 | tail -5
```

Expected: `OK (7 tests, ...)`.

- [ ] **Step 5: Commit**

```bash
git add php/src/TestExecutor.php php/tests/TestExecutorTest.php
git commit -m "feat(php): TestExecutor runs test classes without PHPUnit's TestRunner"
```

---

## Task 5: Rewrite `worker.php` to use TestExecutor

This is the moment the pivot lands. The new worker is ~50 lines.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/php/worker.php`

- [ ] **Step 1: Replace `worker.php`**

```php
<?php

declare(strict_types=1);

// Suppress PHP 8.5 ReflectionProperty::setAccessible deprecation noise; without
// this, the HTML notice would contaminate JSON responses on any path that
// uses reflection. The calls are no-ops since PHP 8.1 anyway.
error_reporting(E_ALL & ~E_DEPRECATED);

require_once __DIR__ . '/vendor/autoload.php';

use PhpunitRust\TestExecutor;

ignore_user_abort(true);

$loadedAutoloads = [];

$handler = static function () use (&$loadedAutoloads): void {
    // Tests run arbitrarily long; PHPUnit's own CLI disables this.
    set_time_limit(0);

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
    $autoload  = (string) $req['autoload'];
    $file      = (string) $req['file'];
    $class     = (string) $req['class'];
    $methods   = (array)  $req['methods'];
    $bootstrap = isset($req['bootstrap']) ? (string) $req['bootstrap'] : null;

    if (!is_file($autoload)) {
        http_response_code(400);
        echo json_encode(['error' => "autoload not found: {$autoload}"]);
        return;
    }
    if (!isset($loadedAutoloads[$autoload])) {
        require_once $autoload;
        if ($bootstrap !== null && is_file($bootstrap)) {
            require_once $bootstrap;
        }
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

    // Capture and discard any stdout the test or bootstrap prints, so it
    // doesn't corrupt the JSON envelope.
    ob_start();
    try {
        $outcomes = TestExecutor::runClass($class, $methods);
        ob_end_clean();
        echo json_encode(['outcomes' => $outcomes]);
    } catch (\Throwable $e) {
        if (ob_get_level() > 0) {
            ob_end_clean();
        }
        http_response_code(500);
        echo json_encode([
            'error'  => 'worker exception while running class',
            'class'  => $class,
            'detail' => $e->getMessage(),
            'trace'  => $e->getTraceAsString(),
        ]);
    }
};

for ($n = 0; $n < 10000; ++$n) {
    $keep = \frankenphp_handle_request($handler);
    gc_collect_cycles();
    if (!$keep) {
        break;
    }
}
```

Notice: no more `Bootstrap`, no more `ResultCollector`, no more `Facade`, no more `Registry::init`, no more `PassedTests` reset, no more reflection on `DeferringDispatcher`. Just receive request → call `TestExecutor::runClass()` → emit JSON.

- [ ] **Step 2: Lint**

```bash
cd /home/gumiranda/PHPUnit_rust/php && php -l worker.php
```

Expected: `No syntax errors detected`.

- [ ] **Step 3: Run our fixture end-to-end (verification)**

Build the release binary and run against the existing fixture:

```bash
cd /home/gumiranda/PHPUnit_rust
cargo build --release 2>&1 | tail -2
pkill -9 -f frankenphp 2>/dev/null; sleep 1
./target/release/phpunit-rust --project fixtures/sample_project 2>&1 | tail -5
echo "exit=$?"
```

Expected summary line:
```
Tests: 15 total, 12 passed, 1 failed, 0 errored, 1 skipped, 1 incomplete, 0 risky (<...>ms)
exit=1
```

These counts must match the v0.2 behavior. The pivot must not regress any of:
- 3 passing tests in CalculatorTest (including expectException via `testDivisionByZeroThrows`)
- 1 pass + 1 fail in FailingTest
- 4 data provider rows in DataProviderTest (all pass)
- 3 dependency-chained tests in DependsTest (all pass)
- 1 skipped + 1 incomplete + 1 pass in SkippedTest

If any number is off, debug **before** committing.

- [ ] **Step 4: Commit**

```bash
cd /home/gumiranda/PHPUnit_rust && git add php/worker.php
git commit -m "feat(worker): bypass PHPUnit TestRunner; delegate to PhpunitRust\\TestExecutor"
```

---

## Task 6: Remove obsolete PHP scaffolding

After Task 5 we no longer use Bootstrap, ResultCollector, or any of the 8 subscriber adapters. Delete them.

**Files:**
- Delete: `/home/gumiranda/PHPUnit_rust/php/src/Bootstrap.php`
- Delete: `/home/gumiranda/PHPUnit_rust/php/src/ResultCollector.php` (contained all 8 adapter classes inline)

- [ ] **Step 1: Delete the files**

```bash
cd /home/gumiranda/PHPUnit_rust
rm php/src/Bootstrap.php php/src/ResultCollector.php
```

- [ ] **Step 2: Confirm nothing references them**

```bash
cd /home/gumiranda/PHPUnit_rust && grep -rn "Bootstrap\|ResultCollector" php/src/ php/worker.php
```

Expected: no output.

- [ ] **Step 3: Re-run the fixture to confirm nothing breaks**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null; sleep 1
./target/release/phpunit-rust --project fixtures/sample_project 2>&1 | grep "^Tests:"
echo "exit=$?"
```

Expected: same summary as Task 5 Step 3.

- [ ] **Step 4: Commit**

```bash
git add -u php/src/
git commit -m "chore(php): drop Bootstrap and ResultCollector; superseded by TestExecutor"
```

---

## Task 7: Wire `--bootstrap` through the CLI

The new worker accepts a `bootstrap` field in the request. Wire the CLI side: auto-detect `phpunit.xml`'s `bootstrap` attribute (via Task 1's parser), let `--bootstrap` flag override.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/types.rs`
- Modify: `/home/gumiranda/PHPUnit_rust/src/runner.rs`
- Modify: `/home/gumiranda/PHPUnit_rust/src/main.rs`

- [ ] **Step 1: Add `bootstrap` to TestRunRequest**

In `src/types.rs`, replace `TestRunRequest` with:

```rust
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestRunRequest {
    pub autoload: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<PathBuf>,
    pub file: PathBuf,
    pub class: String,
    /// Empty vec means "run all test methods in the class".
    pub methods: Vec<String>,
}
```

(The old `phpunit_xml` field is gone — we no longer pass the path to the worker. We parse it on the Rust side and extract just the bootstrap path.)

- [ ] **Step 2: Update the types-module tests**

In `src/types.rs`'s `#[cfg(test)] mod tests`, replace the `run_request_omits_phpunit_xml_when_none` and `run_request_includes_phpunit_xml_when_present` tests with:

```rust
    #[test]
    fn run_request_omits_bootstrap_when_none() {
        let req = TestRunRequest {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            bootstrap: None,
            file: PathBuf::from("/p/tests/Foo.php"),
            class: "App\\Tests\\FooTest".into(),
            methods: vec![],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("bootstrap").is_none());
        assert_eq!(json["class"], "App\\Tests\\FooTest");
    }

    #[test]
    fn run_request_includes_bootstrap_when_present() {
        let req = TestRunRequest {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            bootstrap: Some(PathBuf::from("/p/phpunit.php")),
            file: PathBuf::from("/p/tests/Foo.php"),
            class: "FooTest".into(),
            methods: vec!["testBar".into()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["bootstrap"], "/p/phpunit.php");
        assert_eq!(json["methods"][0], "testBar");
    }
```

- [ ] **Step 3: Update RunConfig and runner.rs**

In `src/runner.rs`, replace `RunConfig` and the request construction in `run()`:

```rust
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub autoload: PathBuf,
    pub bootstrap: Option<PathBuf>,
    pub filter: Option<String>,
}
```

And in `run()`'s loop:

```rust
        let req = TestRunRequest {
            autoload: cfg.autoload.clone(),
            bootstrap: cfg.bootstrap.clone(),
            file,
            class,
            methods,
        };
```

- [ ] **Step 4: Update main.rs**

Replace `src/main.rs`:

```rust
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

use phpunit_rust::client::WorkerClient;
use phpunit_rust::discovery::discover_in_dir;
use phpunit_rust::frankenphp::{find_worker_script, FrankenPhp};
use phpunit_rust::phpunit_xml::parse_bootstrap;
use phpunit_rust::reporter::{print_progress, print_summary};
use phpunit_rust::runner::{run, RunConfig};

#[derive(Parser, Debug)]
#[command(name = "phpunit-rust", version, about = "PHPUnit-compatible test runner via FrankenPHP")]
struct Cli {
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[arg(long, default_value = "tests")]
    tests_dir: PathBuf,
    #[arg(long)]
    filter: Option<String>,
    /// Bootstrap file to require before any tests. Overrides phpunit.xml's
    /// <bootstrap> attribute if both are present.
    #[arg(long)]
    bootstrap: Option<PathBuf>,
    /// Path to phpunit.xml (only used to extract its `bootstrap` attribute).
    /// Defaults to <project>/phpunit.xml or phpunit.xml.dist if found.
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

    let xml_path = match cli.configuration {
        Some(p) => Some(if p.is_absolute() { p } else { project.join(p) }),
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
    let bootstrap = match (cli.bootstrap, xml_path) {
        (Some(b), _) => Some(if b.is_absolute() { b } else { project.join(b) }),
        (None, Some(xml)) => {
            let xml_str = std::fs::read_to_string(&xml)
                .with_context(|| format!("reading {}", xml.display()))?;
            parse_bootstrap(&xml_str).map(|rel| {
                let p = PathBuf::from(&rel);
                if p.is_absolute() { p } else { project.join(p) }
            })
        }
        (None, None) => None,
    };
    if let Some(b) = &bootstrap {
        eprintln!("Using bootstrap: {}", b.display());
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

    let cfg = RunConfig { autoload, bootstrap, filter: cli.filter };
    let report = run(&client, cases, &cfg, |o| print_progress(o))?;
    print_summary(&report);

    if report.is_success() { Ok(ExitCode::SUCCESS) } else { Ok(ExitCode::from(1)) }
}
```

- [ ] **Step 5: Update integration tests**

In `tests/integration.rs`, replace the `request` helper:

```rust
fn request(file: &str, class: &str) -> TestRunRequest {
    let root = fixture_root();
    TestRunRequest {
        autoload: root.join("vendor/autoload.php"),
        bootstrap: None,
        file: root.join(file),
        class: class.into(),
        methods: vec![],
    }
}
```

- [ ] **Step 6: Build and run the full test suite**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null; sleep 1
cargo build --release 2>&1 | tail -2
cargo test 2>&1 | tail -5
```

Expected: clean build, all 15 lib+integration tests pass.

- [ ] **Step 7: End-to-end verify fixture**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null; sleep 1
./target/release/phpunit-rust --project fixtures/sample_project 2>&1 | grep "^Tests:"
echo "exit=$?"
```

Expected: same counts as before (15 / 12 pass / 1 fail / 0 errored / 1 skipped / 1 incomplete / 0 risky / exit=1).

- [ ] **Step 8: Commit**

```bash
git add src/types.rs src/runner.rs src/main.rs tests/integration.rs
git commit -m "feat(cli): pass bootstrap path through to worker; drop phpunit_xml field"
```

---

## Task 8: Big real-world verification — brick/math

We must NOT regress on brick/math (the v0.2 PHPUnit-11 validation case).

**Files:** none — this is a verification gate.

- [ ] **Step 1: Run brick/math through the new architecture**

```bash
cd /home/gumiranda/PHPUnit_rust && pkill -9 -f frankenphp 2>/dev/null; sleep 1
CALCULATOR=Native ./target/release/phpunit-rust --project /tmp/phpunit-rust-smoke/brick-math 2>&1 | grep "^Tests:" | head -1
echo "exit=$?"
```

Expected: `Tests: 13589 total, 13589 passed, 0 failed, 0 errored, 0 skipped, 0 incomplete, 0 risky` and exit=0.

If the count is different, investigate. The pivot must not regress real-world compat.

- [ ] **Step 2: No commit** (verification only).

---

## Task 9: README + plan-index update

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/README.md`

- [ ] **Step 1: Replace README**

```markdown
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
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: README for v0.3.0 — own the runner"
```

---

## Self-Review

**1. Spec coverage** (against user's "Full pivot: own everything except assertions and mocks"):

| Layer | Owned by us? | Task |
|---|---|---|
| Test discovery | ✅ (was already) | n/a |
| `phpunit.xml` bootstrap extraction | ✅ | Task 1 + Task 7 |
| Test instantiation + lifecycle | ✅ | Task 4 (TestExecutor) |
| setUp / tearDown | ✅ | Task 4 |
| setUpBeforeClass / tearDownAfterClass | ✅ | Task 4 |
| Data provider expansion | ✅ | Task 3 (MethodPlanner) + Task 4 |
| @depends ordering + value passing | ✅ | Task 3 + Task 4 |
| expectException handling | ✅ | Task 4 |
| markTestSkipped / markTestIncomplete | ✅ | Task 2 + Task 4 |
| Outcome classification | ✅ | Task 2 (OutcomeBuilder) |
| Assertions (`assertSame` etc.) | **NO — stays in PHPUnit (user-facing)** | by design |
| Mocks (`createMock` etc.) | **NO — stays in PHPUnit (user-facing)** | by design |
| Coverage / reporters / parallel / watch | **OUT OF SCOPE** | follow-up plans |

**2. Placeholder scan:** No TBD, no "implement later", no "handle edge cases" left unscripted. Every code step shows complete code.

**3. Type consistency:**
- `TestRunRequest` adds `bootstrap: Option<PathBuf>` (Task 7), drops `phpunit_xml`. Worker reads `bootstrap` (Task 5). Match.
- `OutcomeBuilder::build(class, method, dataset, durationMs, ?error)` signature matches every call site in Task 4.
- `MethodPlanner::plan` returns `list<Step>` with shape `{method, dataset, args, depends}`. `TestExecutor` consumes exactly those keys.
- Status strings: pass/fail/error/skipped/incomplete (no risky in this plan — risky detection is deferred).

**4. Verification gates:**
- Task 5 Step 3: fixture (15 tests) must match v0.2 counts.
- Task 8: brick/math 13,589 tests must still all pass.

If both gates green, the pivot landed without regression.

## Out-of-scope (deferred to follow-up plans)

- **Parallel execution** — next plan. The pivot removed every shared singleton (Facade, Registry, PassedTests), so the parallel plan now reduces to "spawn N FrankenPHP workers + rayon distribution" with no PHP-side isolation work needed.
- **Risky test detection** — multiple PHPUnit-flavored checks (no assertions, unexpected output, etc.). Worth a dedicated plan; orthogonal to the pivot.
- **Coverage** — PCOV/Xdebug integration. Independent of the runner architecture.
- **JUnit XML / TAP reporters** — new modules in `src/reporter/*`. Independent.
- **Watch mode** — `notify` crate + warm-worker reuse. Independent.
- **Custom extensions / listeners** — likely we'll publish our own extension API rather than implement PHPUnit's.
