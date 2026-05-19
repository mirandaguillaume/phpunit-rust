# PHPUnit_rust MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a minimum-viable Rust test runner that orchestrates FrankenPHP to execute PHPUnit-style tests end-to-end against a fixture suite, proving the architecture before investing in parallelism, coverage, or the long PHPUnit-compatibility tail.

**Architecture:** A Rust binary (`phpunit-rust`) discovers PHP test classes via `tree-sitter-php`, spawns FrankenPHP as a child process running `worker.php` in worker mode, then dispatches one HTTP request per test over a TCP socket to `127.0.0.1:<port>`. The worker bootstraps the user's Composer autoloader once, then invokes test methods directly with manual assertion/exception capture, returning JSON results. Sequential execution only in this MVP; the full PHPUnit `TestRunner` integration, parallelism, and process-isolation fallback are explicitly out of scope and deferred to follow-up plans.

**Tech Stack:**
- Rust 1.75+, edition 2021
- Cargo crates: `clap` 4, `serde`/`serde_json`, `ureq` (sync HTTP), `tree-sitter` + `tree-sitter-php`, `walkdir`, `colored`, `anyhow`, `tempfile` (tests)
- FrankenPHP 1.x (assumed on `$PATH`; verified at startup; ships its own bundled PHP runtime ~8.5)
- PHP 8.1+ on the system (for `composer install`); FrankenPHP's bundled PHP is what actually executes tests at runtime
- PHPUnit 10.x
- Composer for PHP-side autoloading

**Verification notes for the implementer (read before starting):**
- FrankenPHP worker mode API has evolved across releases. Before writing `worker.php` (Task 4), confirm the current `frankenphp_handle_request()` signature and worker-mode env variables at https://frankenphp.dev/docs/worker/ .
- PHPUnit 10 removed `TestResult` in favor of an event-based runner. This MVP intentionally **does not use PHPUnit's runner** — it invokes test methods directly and captures `AssertionFailedError`/`Throwable`. Full runner integration is a follow-up plan. Be explicit about this in the README.
- `tree-sitter-php` exposes two grammars (`tree_sitter_php::language_php()` and `language_php_only()`). We want `language_php()` for files that may include open/close PHP tags.

---

## File Structure

```
PHPUnit_rust/
├── Cargo.toml                    # binary crate manifest
├── README.md                     # usage + MVP scope disclaimers
├── .gitignore                    # target/, vendor/, .phpunit.cache
├── src/
│   ├── main.rs                   # CLI entry point, error reporting
│   ├── lib.rs                    # public surface; re-exports
│   ├── types.rs                  # TestCase, TestOutcome, TestReport
│   ├── discovery.rs              # tree-sitter-based PHP test discovery
│   ├── frankenphp.rs             # FrankenPHP child process supervisor
│   ├── client.rs                 # HTTP client to worker.php
│   ├── runner.rs                 # sequential orchestration
│   └── reporter.rs               # TTY output (dots + summary)
├── php/
│   ├── worker.php                # FrankenPHP worker handler
│   └── composer.json             # PHPUnit dependency for worker bootstrap
├── fixtures/
│   └── sample_project/
│       ├── composer.json
│       ├── src/Calculator.php
│       └── tests/
│           ├── CalculatorTest.php   # 3 passing tests
│           └── FailingTest.php      # 1 passing + 1 failing test
└── tests/
    └── integration.rs            # end-to-end: run binary against fixture
```

**Boundaries:**
- `discovery.rs` is pure: filesystem + parsing only, no network, no FrankenPHP knowledge.
- `frankenphp.rs` owns process lifecycle (spawn, wait-for-ready, shutdown). No HTTP.
- `client.rs` owns the JSON wire protocol with `worker.php`. No process management.
- `runner.rs` is the only file that knows about all three — it composes them.
- `reporter.rs` consumes `TestReport` values; no I/O dependencies beyond stdout.

---

## Task 1: Initialize Cargo project + git

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/Cargo.toml`
- Create: `/home/gumiranda/PHPUnit_rust/src/main.rs`
- Create: `/home/gumiranda/PHPUnit_rust/.gitignore`

- [ ] **Step 1: Initialize git repo**

Run from `/home/gumiranda/PHPUnit_rust`:

```bash
git init
git config user.email "guillaume11miranda@gmail.com"
git config user.name "Guillaume Miranda"
```

Expected: `Initialized empty Git repository in /home/gumiranda/PHPUnit_rust/.git/`

- [ ] **Step 2: Create `Cargo.toml`**

Write `/home/gumiranda/PHPUnit_rust/Cargo.toml`:

```toml
[package]
name = "phpunit-rust"
version = "0.1.0"
edition = "2021"
description = "A fast PHPUnit-compatible test runner orchestrating FrankenPHP from Rust"
license = "MIT"

[[bin]]
name = "phpunit-rust"
path = "src/main.rs"

[lib]
path = "src/lib.rs"

[dependencies]
anyhow = "1.0"
clap = { version = "4.5", features = ["derive"] }
colored = "2.1"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
tree-sitter = "0.22"
tree-sitter-php = "0.22"
ureq = { version = "2.10", features = ["json"] }
walkdir = "2.5"

[dev-dependencies]
tempfile = "3.10"
```

- [ ] **Step 3: Create `.gitignore`**

Write `/home/gumiranda/PHPUnit_rust/.gitignore`:

```
/target
**/vendor/
**/.phpunit.cache/
**/composer.lock
*.swp
.DS_Store
```

- [ ] **Step 4: Create minimal `src/main.rs` to verify it builds**

Write `/home/gumiranda/PHPUnit_rust/src/main.rs`:

```rust
fn main() {
    println!("phpunit-rust 0.1.0");
}
```

Write `/home/gumiranda/PHPUnit_rust/src/lib.rs`:

```rust
//! phpunit-rust library surface.
```

- [ ] **Step 5: Build to verify the toolchain**

Run from `/home/gumiranda/PHPUnit_rust`:

```bash
cargo build
```

Expected: `Finished \`dev\` profile [unoptimized + debuginfo] target(s)` with no errors. Crate downloads will happen on first build.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml .gitignore src/main.rs src/lib.rs
git commit -m "chore: initialize Cargo project skeleton"
```

---

## Task 2: Fixture PHP project (test target)

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/composer.json`
- Create: `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/src/Calculator.php`
- Create: `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/CalculatorTest.php`
- Create: `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/FailingTest.php`

- [ ] **Step 1: Create fixture `composer.json`**

Write `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/composer.json`:

```json
{
    "name": "phpunit-rust/sample",
    "type": "project",
    "autoload": {
        "psr-4": {
            "Sample\\": "src/"
        }
    },
    "autoload-dev": {
        "psr-4": {
            "Sample\\Tests\\": "tests/"
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

- [ ] **Step 2: Create a trivial subject-under-test**

Write `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/src/Calculator.php`:

```php
<?php

declare(strict_types=1);

namespace Sample;

final class Calculator
{
    public function add(int $a, int $b): int
    {
        return $a + $b;
    }

    public function divide(int $a, int $b): int
    {
        if ($b === 0) {
            throw new \DivisionByZeroError('cannot divide by zero');
        }
        return intdiv($a, $b);
    }
}
```

- [ ] **Step 3: Create passing test class**

Write `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/CalculatorTest.php`:

```php
<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\TestCase;
use Sample\Calculator;

final class CalculatorTest extends TestCase
{
    public function testAddsTwoPositiveIntegers(): void
    {
        $calc = new Calculator();
        $this->assertSame(5, $calc->add(2, 3));
    }

    public function testAddsNegatives(): void
    {
        $calc = new Calculator();
        $this->assertSame(-7, $calc->add(-3, -4));
    }

    public function testDivisionByZeroThrows(): void
    {
        $calc = new Calculator();
        $this->expectException(\DivisionByZeroError::class);
        $calc->divide(1, 0);
    }
}
```

- [ ] **Step 4: Create a mixed-outcome test class**

Write `/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/FailingTest.php`:

```php
<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\TestCase;

final class FailingTest extends TestCase
{
    public function testThisPasses(): void
    {
        $this->assertTrue(true);
    }

    public function testThisDeliberatelyFails(): void
    {
        $this->assertSame(1, 2, 'this is intentional for runner testing');
    }
}
```

- [ ] **Step 5: Install fixture dependencies**

Run from `/home/gumiranda/PHPUnit_rust/fixtures/sample_project`:

```bash
composer install --no-interaction
```

Expected: `vendor/` directory created with `phpunit/phpunit` installed. If composer is missing, plan installation pause — the implementer must have composer available.

- [ ] **Step 6: Verify vanilla PHPUnit runs against the fixture (sanity baseline)**

Run from `/home/gumiranda/PHPUnit_rust/fixtures/sample_project`:

```bash
./vendor/bin/phpunit tests
```

Expected: 5 tests, 4 passing, 1 failing (`testThisDeliberatelyFails`). This is our oracle for what our Rust runner must reproduce.

- [ ] **Step 7: Commit**

```bash
git add fixtures/
git commit -m "test: add Calculator fixture project with passing + failing tests"
```

---

## Task 3: PHP worker dependencies

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/php/composer.json`

- [ ] **Step 1: Create `php/composer.json`**

The worker itself needs PHPUnit available so `TestCase` and `AssertionFailedError` resolve. It will require/include the **user project's** autoloader at runtime, but we still need PHPUnit's classes loadable.

Write `/home/gumiranda/PHPUnit_rust/php/composer.json`:

```json
{
    "name": "phpunit-rust/worker",
    "type": "project",
    "description": "FrankenPHP worker that executes PHPUnit tests on behalf of phpunit-rust",
    "require": {
        "php": ">=8.1",
        "phpunit/phpunit": "^10.5"
    }
}
```

- [ ] **Step 2: Install worker dependencies**

Run from `/home/gumiranda/PHPUnit_rust/php`:

```bash
composer install --no-interaction
```

Expected: `php/vendor/` populated; `phpunit/phpunit` available.

- [ ] **Step 3: Commit**

```bash
git add php/composer.json
git commit -m "build: add PHPUnit dependency for worker bootstrap"
```

---

## Task 4: FrankenPHP worker — handshake only

This task gets the worker accepting a request and echoing back JSON, with no PHPUnit logic yet. We prove the FrankenPHP wire works end-to-end before adding test execution.

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/php/worker.php`

- [ ] **Step 1: Write the minimal worker handler**

Write `/home/gumiranda/PHPUnit_rust/php/worker.php`:

```php
<?php

declare(strict_types=1);

require_once __DIR__ . '/vendor/autoload.php';

ignore_user_abort(true);

$handler = static function (): void {
    $raw = file_get_contents('php://input');
    $req = json_decode($raw, true);

    header('Content-Type: application/json');

    if (!is_array($req)) {
        http_response_code(400);
        echo json_encode(['error' => 'request body must be a JSON object']);
        return;
    }

    echo json_encode([
        'ok' => true,
        'echo' => $req,
        'phpunit_version' => \PHPUnit\Runner\Version::id(),
    ]);
};

for ($nbHandledRequests = 0; $nbHandledRequests < 1000; ++$nbHandledRequests) {
    $keepRunning = \frankenphp_handle_request($handler);
    gc_collect_cycles();
    if (!$keepRunning) {
        break;
    }
}
```

- [ ] **Step 2: Smoke-test the worker manually**

Run from `/home/gumiranda/PHPUnit_rust/php`:

```bash
FRANKENPHP_CONFIG="worker ./worker.php" frankenphp php-server --listen 127.0.0.1:8765 --root . &
sleep 1
curl -s -X POST http://127.0.0.1:8765/ -d '{"hello":"world"}'
```

Expected output (formatted): `{"ok":true,"echo":{"hello":"world"},"phpunit_version":"10.5.x"}`

- [ ] **Step 3: Stop the manual smoke test**

```bash
pkill -f frankenphp
```

- [ ] **Step 4: Commit**

```bash
git add php/worker.php
git commit -m "feat(worker): handshake echo handler verifying FrankenPHP wire"
```

---

## Task 5: Worker executes a test method

Extend the worker to actually load the user's autoloader, instantiate a test class, invoke a method, and return a structured result. This is the load-bearing PHP-side logic.

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/php/worker.php`

- [ ] **Step 1: Rewrite worker.php with test execution**

Replace the contents of `/home/gumiranda/PHPUnit_rust/php/worker.php` with:

```php
<?php

declare(strict_types=1);

require_once __DIR__ . '/vendor/autoload.php';

ignore_user_abort(true);

use PHPUnit\Framework\AssertionFailedError;
use PHPUnit\Framework\ExpectationFailedException;
use PHPUnit\Framework\TestCase;

$loadedProjects = [];

$handler = static function () use (&$loadedProjects): void {
    $raw = file_get_contents('php://input');
    $req = json_decode($raw, true);

    header('Content-Type: application/json');

    if (!is_array($req) || !isset($req['autoload'], $req['file'], $req['class'], $req['method'])) {
        http_response_code(400);
        echo json_encode(['error' => 'missing autoload, file, class, or method']);
        return;
    }

    $autoload = $req['autoload'];
    if (!isset($loadedProjects[$autoload])) {
        if (!is_file($autoload)) {
            http_response_code(400);
            echo json_encode(['error' => "autoload not found: $autoload"]);
            return;
        }
        require_once $autoload;
        $loadedProjects[$autoload] = true;
    }

    if (!is_file($req['file'])) {
        http_response_code(400);
        echo json_encode(['error' => "test file not found: " . $req['file']]);
        return;
    }
    require_once $req['file'];

    $class = $req['class'];
    $method = $req['method'];

    if (!class_exists($class)) {
        http_response_code(404);
        echo json_encode(['error' => "class $class not found after loading " . $req['file']]);
        return;
    }

    if (!is_subclass_of($class, TestCase::class)) {
        http_response_code(400);
        echo json_encode(['error' => "$class does not extend PHPUnit\\Framework\\TestCase"]);
        return;
    }

    $test = new $class($method);

    $status = 'pass';
    $message = null;
    $trace = null;
    $startedAt = microtime(true);

    try {
        // TestCase always provides setUp/tearDown (empty by default), so no
        // existence check is needed. Closure::bind is used instead of
        // ReflectionMethod::setAccessible() because PHP 8.4+ deprecates the
        // latter for protected methods, and the deprecation warning would
        // contaminate the JSON response body.
        Closure::bind(fn () => $this->setUp(), $test, $test)();
        $test->{$method}();
        Closure::bind(fn () => $this->tearDown(), $test, $test)();
    } catch (ExpectationFailedException $e) {
        $status = 'fail';
        $message = $e->getMessage();
        $trace = $e->getTraceAsString();
    } catch (AssertionFailedError $e) {
        $status = 'fail';
        $message = $e->getMessage();
        $trace = $e->getTraceAsString();
    } catch (\Throwable $e) {
        $status = 'error';
        $message = get_class($e) . ': ' . $e->getMessage();
        $trace = $e->getTraceAsString();
    }

    echo json_encode([
        'class' => $class,
        'method' => $method,
        'status' => $status,
        'message' => $message,
        'trace' => $trace,
        'duration_ms' => (microtime(true) - $startedAt) * 1000.0,
    ]);
};

for ($n = 0; $n < 10000; ++$n) {
    $keep = \frankenphp_handle_request($handler);
    gc_collect_cycles();
    if (!$keep) {
        break;
    }
}
```

**Notes on this design (do not change without discussion):**
- `$loadedProjects` caches per-autoload-path so re-running tests against the same project doesn't re-require the autoloader.
- MVP captures `expectException` *only when the user's test already declared it*; uncaught exceptions become `error` status. PHPUnit's `expectException()` machinery actually runs in `TestCase::runTest()`, which we are **not** invoking — so the fixture's `testDivisionByZeroThrows` will currently report `error`. **This is a known MVP limitation**; flag in README. Full PHPUnit Runner integration (a future plan) fixes it.
- Worker handles up to 10000 requests then exits, allowing FrankenPHP to recycle (defense against memory leaks).

- [ ] **Step 2: Manual smoke test with a real test class**

Run from `/home/gumiranda/PHPUnit_rust/php`:

```bash
FRANKENPHP_CONFIG="worker ./worker.php" frankenphp php-server --listen 127.0.0.1:8765 --root . &
sleep 1
curl -s -X POST http://127.0.0.1:8765/ \
  -H 'Content-Type: application/json' \
  -d "$(cat <<JSON
{
  "autoload": "/home/gumiranda/PHPUnit_rust/fixtures/sample_project/vendor/autoload.php",
  "file": "/home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests/CalculatorTest.php",
  "class": "Sample\\\\Tests\\\\CalculatorTest",
  "method": "testAddsTwoPositiveIntegers"
}
JSON
)"
```

Expected: JSON response with `"status":"pass"`, `"duration_ms": <small number>`.

Run again with `"method":"testAddsTwoPositiveIntegers"` replaced by `"testThisDeliberatelyFails"` and class `"Sample\\\\Tests\\\\FailingTest"`, file path adjusted — expected `"status":"fail"`.

- [ ] **Step 3: Stop FrankenPHP**

```bash
pkill -f frankenphp
```

- [ ] **Step 4: Commit**

```bash
git add php/worker.php
git commit -m "feat(worker): execute PHPUnit test method and return structured result"
```

---

## Task 6: Rust shared types

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/src/types.rs`
- Modify: `/home/gumiranda/PHPUnit_rust/src/lib.rs`

- [ ] **Step 1: Write failing unit test for serialization**

Write `/home/gumiranda/PHPUnit_rust/src/types.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    pub file: PathBuf,
    pub class: String,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestRequest {
    pub autoload: PathBuf,
    pub file: PathBuf,
    pub class: String,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    Pass,
    Fail,
    Error,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TestOutcome {
    pub class: String,
    pub method: String,
    pub status: TestStatus,
    pub message: Option<String>,
    pub trace: Option<String>,
    pub duration_ms: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serializes_with_expected_keys() {
        let req = TestRequest {
            autoload: PathBuf::from("/p/vendor/autoload.php"),
            file: PathBuf::from("/p/tests/Foo.php"),
            class: "App\\Tests\\FooTest".into(),
            method: "testBar".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["autoload"], "/p/vendor/autoload.php");
        assert_eq!(json["class"], "App\\\\Tests\\\\FooTest");
        assert_eq!(json["method"], "testBar");
    }

    #[test]
    fn test_outcome_deserializes_pass() {
        let raw = r#"{"class":"A","method":"b","status":"pass","message":null,"trace":null,"duration_ms":1.5}"#;
        let outcome: TestOutcome = serde_json::from_str(raw).unwrap();
        assert_eq!(outcome.status, TestStatus::Pass);
        assert_eq!(outcome.duration_ms, 1.5);
    }

    #[test]
    fn test_outcome_deserializes_fail_with_message() {
        let raw = r#"{"class":"A","method":"b","status":"fail","message":"oops","trace":"#0 ...","duration_ms":0.3}"#;
        let outcome: TestOutcome = serde_json::from_str(raw).unwrap();
        assert_eq!(outcome.status, TestStatus::Fail);
        assert_eq!(outcome.message.as_deref(), Some("oops"));
    }
}
```

- [ ] **Step 2: Update `lib.rs` to expose the module**

Replace `/home/gumiranda/PHPUnit_rust/src/lib.rs` with:

```rust
//! phpunit-rust library surface.

pub mod types;
```

- [ ] **Step 3: Run the tests to verify they pass**

```bash
cargo test --lib types
```

Expected: `test result: ok. 3 passed; 0 failed`.

- [ ] **Step 4: Commit**

```bash
git add src/types.rs src/lib.rs
git commit -m "feat(types): define wire types for runner ↔ worker protocol"
```

---

## Task 7: FrankenPHP process supervisor

Spawn FrankenPHP, wait until it's accepting connections, shut it down cleanly when dropped.

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/src/frankenphp.rs`
- Modify: `/home/gumiranda/PHPUnit_rust/src/lib.rs`

- [ ] **Step 1: Write `frankenphp.rs`**

Write `/home/gumiranda/PHPUnit_rust/src/frankenphp.rs`:

```rust
use anyhow::{anyhow, Context, Result};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct FrankenPhp {
    child: Child,
    pub base_url: String,
}

impl FrankenPhp {
    /// Spawn FrankenPHP in worker mode bound to a free localhost port.
    /// `worker_script` must be an absolute path to `worker.php`.
    pub fn spawn(worker_script: &Path) -> Result<Self> {
        if !worker_script.is_file() {
            return Err(anyhow!("worker script not found: {}", worker_script.display()));
        }

        let port = find_free_port()?;
        let root = worker_script
            .parent()
            .ok_or_else(|| anyhow!("worker script has no parent dir"))?;

        let child = Command::new("frankenphp")
            .arg("php-server")
            .arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--root")
            .arg(root)
            .arg("--worker")
            .arg(worker_script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn frankenphp; is it on $PATH?")?;

        let base_url = format!("http://127.0.0.1:{port}");
        let inst = FrankenPhp { child, base_url };
        inst.wait_until_ready(port, Duration::from_secs(10))?;
        Ok(inst)
    }

    fn wait_until_ready(&self, port: u16, timeout: Duration) -> Result<()> {
        // Use an HTTP probe rather than a bare TCP probe. The TCP port opens
        // as soon as Caddy binds, but the PHP worker may not be ready to
        // handle requests until slightly later. We send a lightweight GET
        // probe; the worker returns 400 (missing fields) or 200 — either
        // way it proves the worker is alive and responsive.
        let deadline = Instant::now() + timeout;
        let probe_url = format!("http://127.0.0.1:{port}/worker.php");
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(500))
            .build();
        while Instant::now() < deadline {
            match agent.get(&probe_url).call() {
                Ok(_) => return Ok(()),
                Err(ureq::Error::Status(_, _)) => return Ok(()), // any HTTP status = worker ready
                Err(_) => {}                                      // connection refused or transport error → retry
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(anyhow!("frankenphp did not become ready within {timeout:?}"))
    }

    pub fn worker_url(&self) -> String {
        // FrankenPHP routes by file path in worker mode; the URL must reference
        // the worker script itself, not just `/`. Discovered during Task 4 smoke test.
        format!("{}/worker.php", self.base_url)
    }
}

impl Drop for FrankenPhp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

pub fn find_worker_script() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    candidates.push(PathBuf::from("php/worker.php"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // release build: target/release/phpunit-rust → ../../php/worker.php
            // debug build:   target/debug/phpunit-rust   → ../../php/worker.php
            candidates.push(dir.join("../../php/worker.php"));
            // installed alongside binary
            candidates.push(dir.join("php/worker.php"));
        }
    }
    for c in &candidates {
        if c.is_file() {
            return Ok(c.canonicalize()?);
        }
    }
    Err(anyhow!(
        "worker.php not found in any of: {:?}",
        candidates
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_free_port_returns_usable_port() {
        let port = find_free_port().unwrap();
        // Bind it again to confirm it's actually free.
        let _ = TcpListener::bind(("127.0.0.1", port)).unwrap();
    }
}
```

- [ ] **Step 2: Register the module**

Replace `/home/gumiranda/PHPUnit_rust/src/lib.rs` with:

```rust
//! phpunit-rust library surface.

pub mod frankenphp;
pub mod types;
```

- [ ] **Step 3: Run unit tests**

```bash
cargo test --lib frankenphp
```

Expected: `test result: ok. 1 passed; 0 failed`.

- [ ] **Step 4: Commit**

```bash
git add src/frankenphp.rs src/lib.rs
git commit -m "feat(frankenphp): spawn + readiness-wait + drop-cleanup supervisor"
```

---

## Task 8: HTTP client to worker.php

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/src/client.rs`
- Modify: `/home/gumiranda/PHPUnit_rust/src/lib.rs`

- [ ] **Step 1: Write the client**

Write `/home/gumiranda/PHPUnit_rust/src/client.rs`:

```rust
use crate::types::{TestOutcome, TestRequest};
use anyhow::{anyhow, Context, Result};
use std::time::Duration;

pub struct WorkerClient {
    url: String,
    agent: ureq::Agent,
}

impl WorkerClient {
    pub fn new(url: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .build();
        Self { url: url.into(), agent }
    }

    pub fn run_test(&self, req: &TestRequest) -> Result<TestOutcome> {
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
        let outcome: TestOutcome = resp.into_json().context("worker response was not valid JSON")?;
        Ok(outcome)
    }
}
```

- [ ] **Step 2: Register the module**

Replace `/home/gumiranda/PHPUnit_rust/src/lib.rs` with:

```rust
//! phpunit-rust library surface.

pub mod client;
pub mod frankenphp;
pub mod types;
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo build
```

Expected: no warnings on the new module.

- [ ] **Step 4: Commit**

```bash
git add src/client.rs src/lib.rs
git commit -m "feat(client): ureq-based JSON client for worker.php"
```

---

## Task 9: Integration test for hardcoded test execution

Before adding discovery, prove the supervisor + client + worker actually run a real test together.

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/tests/integration.rs`

- [ ] **Step 1: Write an integration test**

Write `/home/gumiranda/PHPUnit_rust/tests/integration.rs`:

```rust
use phpunit_rust::client::WorkerClient;
use phpunit_rust::frankenphp::{find_worker_script, FrankenPhp};
use phpunit_rust::types::{TestRequest, TestStatus};
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project")
}

#[test]
fn runs_a_passing_test_end_to_end() {
    let worker = find_worker_script().expect("worker.php must exist");
    let fph = FrankenPhp::spawn(&worker).expect("frankenphp must spawn");
    let client = WorkerClient::new(fph.worker_url());

    let root = fixture_root();
    let req = TestRequest {
        autoload: root.join("vendor/autoload.php"),
        file: root.join("tests/CalculatorTest.php"),
        class: "Sample\\Tests\\CalculatorTest".into(),
        method: "testAddsTwoPositiveIntegers".into(),
    };

    let outcome = client.run_test(&req).expect("worker call must succeed");
    assert_eq!(outcome.status, TestStatus::Pass, "outcome was: {outcome:?}");
}

#[test]
fn reports_a_failing_test_as_fail() {
    let worker = find_worker_script().expect("worker.php must exist");
    let fph = FrankenPhp::spawn(&worker).expect("frankenphp must spawn");
    let client = WorkerClient::new(fph.worker_url());

    let root = fixture_root();
    let req = TestRequest {
        autoload: root.join("vendor/autoload.php"),
        file: root.join("tests/FailingTest.php"),
        class: "Sample\\Tests\\FailingTest".into(),
        method: "testThisDeliberatelyFails".into(),
    };

    let outcome = client.run_test(&req).expect("worker call must succeed");
    assert_eq!(outcome.status, TestStatus::Fail);
    assert!(outcome.message.as_deref().unwrap_or("").contains("intentional"));
}
```

- [ ] **Step 2: Run the integration tests**

```bash
cargo test --test integration -- --test-threads=1
```

(`--test-threads=1` because each test spawns its own FrankenPHP on a free port; running them serially keeps logs interpretable for now. Parallel-safe spawning is fine in principle since we find a free port per test.)

Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add tests/integration.rs
git commit -m "test: end-to-end smoke test runs PHPUnit fixture via FrankenPHP"
```

---

## Task 10: Discovery — parse a single PHP file

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/src/discovery.rs`
- Modify: `/home/gumiranda/PHPUnit_rust/src/lib.rs`

- [ ] **Step 1: Write failing tests for discovery**

Write `/home/gumiranda/PHPUnit_rust/src/discovery.rs`:

```rust
use crate::types::TestCase;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Query, QueryCursor};
use walkdir::WalkDir;

/// Parse one PHP file and return any test classes + methods it declares.
pub fn discover_in_file(path: &Path) -> Result<Vec<TestCase>> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_php::language_php())
        .context("setting tree-sitter-php language")?;
    let tree = parser
        .parse(&src, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse {}", path.display()))?;

    let root = tree.root_node();
    let bytes = src.as_bytes();

    let namespace = find_namespace(root, bytes);

    let mut cases = Vec::new();
    collect_test_classes(root, bytes, namespace.as_deref(), path, &mut cases)?;
    Ok(cases)
}

fn find_namespace(root: Node, bytes: &[u8]) -> Option<String> {
    // PHP: `namespace Foo\Bar;` produces a `namespace_definition` node.
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "namespace_definition" {
            if let Some(name) = child.child_by_field_name("name") {
                return name.utf8_text(bytes).ok().map(String::from);
            }
        }
    }
    None
}

fn collect_test_classes(
    root: Node,
    bytes: &[u8],
    namespace: Option<&str>,
    path: &Path,
    out: &mut Vec<TestCase>,
) -> Result<()> {
    let query_src = r#"
        (class_declaration
          name: (name) @class_name
          (base_clause (name) @base)?
          body: (declaration_list) @body)
    "#;
    let lang = tree_sitter_php::language_php();
    let query = Query::new(&lang, query_src).context("compiling class query")?;
    let mut cursor = QueryCursor::new();
    let captures = query.capture_names();
    let class_name_idx = captures.iter().position(|n| *n == "class_name").unwrap();
    let base_idx = captures.iter().position(|n| *n == "base").unwrap();
    let body_idx = captures.iter().position(|n| *n == "body").unwrap();

    for m in cursor.matches(&query, root, bytes) {
        let mut class_name: Option<&str> = None;
        let mut base_name: Option<&str> = None;
        let mut body_node: Option<Node> = None;
        for cap in m.captures {
            let idx = cap.index as usize;
            if idx == class_name_idx {
                class_name = cap.node.utf8_text(bytes).ok();
            } else if idx == base_idx {
                base_name = cap.node.utf8_text(bytes).ok();
            } else if idx == body_idx {
                body_node = Some(cap.node);
            }
        }

        let (Some(name), Some(body)) = (class_name, body_node) else { continue };
        let base = base_name.unwrap_or("");
        if !is_testcase_subclass(base) {
            continue;
        }

        let fqcn = match namespace {
            Some(ns) => format!("{ns}\\{name}"),
            None => name.to_string(),
        };

        for method in collect_test_methods(body, bytes) {
            out.push(TestCase {
                file: path.to_path_buf(),
                class: fqcn.clone(),
                method,
            });
        }
    }
    Ok(())
}

fn is_testcase_subclass(base: &str) -> bool {
    // MVP heuristic: anything named TestCase or ending in TestCase.
    // Real PHPUnit-compat would resolve `use` aliases. Tracked in follow-up plan.
    base == "TestCase" || base.ends_with("\\TestCase")
}

fn collect_test_methods(body: Node, bytes: &[u8]) -> Vec<String> {
    let mut methods = Vec::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        // Skip non-public for MVP — PHPUnit only runs public methods.
        let is_public = method_is_public(child, bytes);
        if !is_public {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else { continue };
        let Ok(name) = name_node.utf8_text(bytes) else { continue };
        if name.starts_with("test") {
            methods.push(name.to_string());
        }
        // #[Test] attribute support is deferred to a follow-up plan.
    }
    methods
}

fn method_is_public(method: Node, bytes: &[u8]) -> bool {
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return child.utf8_text(bytes).map(|t| t == "public").unwrap_or(false);
        }
    }
    // PHP defaults to public when no visibility modifier is present.
    true
}

/// Walk a directory, returning all discovered test cases.
pub fn discover_in_dir(root: &Path) -> Result<Vec<TestCase>> {
    let mut all = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("php") {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.contains("Test") {
            continue;
        }
        all.extend(discover_in_file(p)?);
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SomeTest.php");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        (dir, path)
    }

    #[test]
    fn discovers_a_namespaced_test_class() {
        let src = r#"<?php
namespace App\Tests;
use PHPUnit\Framework\TestCase;
final class FooTest extends TestCase {
    public function testOne(): void {}
    public function testTwo(): void {}
    public function helper(): void {}
    private function testIsPrivateSoSkipped(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let methods: Vec<_> = cases.iter().map(|c| c.method.as_str()).collect();
        assert_eq!(methods, vec!["testOne", "testTwo"]);
        assert_eq!(cases[0].class, "App\\Tests\\FooTest");
    }

    #[test]
    fn skips_classes_not_extending_testcase() {
        let src = r#"<?php
namespace App;
final class NotATest {
    public function testNothing(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert!(cases.is_empty());
    }

    #[test]
    fn handles_file_without_namespace() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class BareTest extends TestCase {
    public function testStuff(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].class, "BareTest");
    }
}
```

- [ ] **Step 2: Register the module**

Replace `/home/gumiranda/PHPUnit_rust/src/lib.rs` with:

```rust
//! phpunit-rust library surface.

pub mod client;
pub mod discovery;
pub mod frankenphp;
pub mod types;
```

- [ ] **Step 3: Run discovery tests**

```bash
cargo test --lib discovery
```

Expected: `test result: ok. 3 passed; 0 failed`.

- [ ] **Step 4: Verify against the fixture project**

Add a temporary debug test at the end of `src/discovery.rs` `mod tests`:

```rust
    #[test]
    fn discovers_fixture_project_tests() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/sample_project/tests");
        let cases = discover_in_dir(&fixture).unwrap();
        let methods: Vec<_> = cases.iter().map(|c| (c.class.as_str(), c.method.as_str())).collect();
        assert!(methods.contains(&("Sample\\Tests\\CalculatorTest", "testAddsTwoPositiveIntegers")));
        assert!(methods.contains(&("Sample\\Tests\\FailingTest", "testThisDeliberatelyFails")));
        assert_eq!(cases.len(), 5);
    }
```

Run:

```bash
cargo test --lib discovery
```

Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add src/discovery.rs src/lib.rs
git commit -m "feat(discovery): tree-sitter-based test class + method discovery"
```

---

## Task 11: Sequential runner

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/src/runner.rs`
- Modify: `/home/gumiranda/PHPUnit_rust/src/lib.rs`

- [ ] **Step 1: Write `runner.rs`**

Write `/home/gumiranda/PHPUnit_rust/src/runner.rs`:

```rust
use crate::client::WorkerClient;
use crate::types::{TestCase, TestOutcome, TestRequest};
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub autoload: PathBuf,
    pub filter: Option<String>,
}

#[derive(Debug)]
pub struct Report {
    pub outcomes: Vec<TestOutcome>,
    pub total_duration_ms: f64,
}

impl Report {
    pub fn passed(&self) -> usize {
        self.outcomes.iter().filter(|o| matches!(o.status, crate::types::TestStatus::Pass)).count()
    }
    pub fn failed(&self) -> usize {
        self.outcomes.iter().filter(|o| matches!(o.status, crate::types::TestStatus::Fail)).count()
    }
    pub fn errored(&self) -> usize {
        self.outcomes.iter().filter(|o| matches!(o.status, crate::types::TestStatus::Error)).count()
    }
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
    let mut outcomes = Vec::new();
    let mut total = 0.0;
    for case in cases {
        if let Some(filter) = &cfg.filter {
            let fqn = format!("{}::{}", case.class, case.method);
            if !fqn.contains(filter) {
                continue;
            }
        }
        let req = TestRequest {
            autoload: cfg.autoload.clone(),
            file: case.file.clone(),
            class: case.class.clone(),
            method: case.method.clone(),
        };
        let outcome = client.run_test(&req)?;
        total += outcome.duration_ms;
        on_progress(&outcome);
        outcomes.push(outcome);
    }
    Ok(Report { outcomes, total_duration_ms: total })
}
```

- [ ] **Step 2: Register the module**

Replace `/home/gumiranda/PHPUnit_rust/src/lib.rs` with:

```rust
//! phpunit-rust library surface.

pub mod client;
pub mod discovery;
pub mod frankenphp;
pub mod reporter;
pub mod runner;
pub mod types;
```

(We'll create `reporter.rs` in the next task; the `mod` line stays — `cargo build` will fail until then, which is expected.)

- [ ] **Step 3: Build (expect failure)**

```bash
cargo build
```

Expected: error `file not found for module 'reporter'`. This is intentional; Task 12 closes the loop.

---

## Task 12: TTY reporter

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/src/reporter.rs`

- [ ] **Step 1: Write the reporter**

Write `/home/gumiranda/PHPUnit_rust/src/reporter.rs`:

```rust
use crate::runner::Report;
use crate::types::{TestOutcome, TestStatus};
use colored::Colorize;

pub fn print_progress(outcome: &TestOutcome) {
    let mark = match outcome.status {
        TestStatus::Pass => ".".green(),
        TestStatus::Fail => "F".red(),
        TestStatus::Error => "E".yellow(),
    };
    print!("{mark}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

pub fn print_summary(report: &Report) {
    println!();
    println!();
    for outcome in &report.outcomes {
        match outcome.status {
            TestStatus::Pass => {}
            TestStatus::Fail | TestStatus::Error => {
                let label = if matches!(outcome.status, TestStatus::Fail) { "FAIL" } else { "ERROR" };
                let color = if matches!(outcome.status, TestStatus::Fail) {
                    label.red().bold()
                } else {
                    label.yellow().bold()
                };
                println!("{color}  {}::{}", outcome.class, outcome.method);
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
        }
    }
    let p = report.passed();
    let f = report.failed();
    let e = report.errored();
    let total = report.outcomes.len();
    let line = format!(
        "Tests: {total} total, {} passed, {} failed, {} errored ({:.1}ms)",
        p, f, e, report.total_duration_ms
    );
    if report.is_success() {
        println!("{}", line.green().bold());
    } else {
        println!("{}", line.red().bold());
    }
}
```

- [ ] **Step 2: Build the workspace**

```bash
cargo build
```

Expected: clean build, no warnings.

- [ ] **Step 3: Commit Tasks 11 + 12 together**

```bash
git add src/runner.rs src/reporter.rs src/lib.rs
git commit -m "feat(runner,reporter): sequential orchestration + TTY output"
```

---

## Task 13: CLI wiring

**Files:**
- Modify: `/home/gumiranda/PHPUnit_rust/src/main.rs`

- [ ] **Step 1: Replace `main.rs` with the full CLI**

Write `/home/gumiranda/PHPUnit_rust/src/main.rs`:

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
#[command(name = "phpunit-rust", version, about = "PHPUnit-compatible test runner via FrankenPHP (MVP)")]
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

    eprintln!("Discovering tests in {}...", tests_dir.display());
    let cases = discover_in_dir(&tests_dir)?;
    eprintln!("Found {} tests.", cases.len());

    let worker = find_worker_script()?;
    let fph = FrankenPhp::spawn(&worker)?;
    let client = WorkerClient::new(fph.worker_url());

    let cfg = RunConfig { autoload, filter: cli.filter };
    let report = run(&client, cases, &cfg, |o| print_progress(o))?;
    print_summary(&report);

    if report.is_success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}
```

- [ ] **Step 2: Build the binary**

```bash
cargo build --release
```

Expected: `target/release/phpunit-rust` exists.

- [ ] **Step 3: Run against the fixture**

```bash
./target/release/phpunit-rust --project fixtures/sample_project
```

Expected output (approximately):

```
Discovering tests in /home/gumiranda/PHPUnit_rust/fixtures/sample_project/tests...
Found 5 tests.
....F

FAIL  Sample\Tests\FailingTest::testThisDeliberatelyFails
    Failed asserting that 2 is identical to 1.
    this is intentional for runner testing
    ...
Tests: 5 total, 3 passed, 1 failed, 1 errored (<duration>ms)
```

**Note on the 1 errored:** `testDivisionByZeroThrows` will report `error` because the MVP worker does **not** wire up `expectException`. This is the known limitation flagged in Task 5. The summary line and exit code (non-zero) are still correct.

- [ ] **Step 4: Run with a filter**

```bash
./target/release/phpunit-rust --project fixtures/sample_project --filter testAdds
```

Expected: only the two `testAdds*` tests run, both pass, exit 0.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat(cli): clap-based CLI with --project, --tests-dir, --filter"
```

---

## Task 14: README documenting scope, limitations, and usage

**Files:**
- Create: `/home/gumiranda/PHPUnit_rust/README.md`

- [ ] **Step 1: Write the README**

Write `/home/gumiranda/PHPUnit_rust/README.md`:

```markdown
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
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: README with MVP scope, limitations, and usage"
```

---

## Self-Review

**Spec coverage:**
- Architecture decision (Rust orchestrator + FrankenPHP-as-subprocess) — implemented across Tasks 4–13.
- Test isolation risk — explicitly flagged but **deferred**; MVP relies on PHP's fresh-class-instance-per-request for minimal isolation. Documented in README. A follow-up plan must address proper between-test state reset before parallelism is introduced.
- Worker mode autoloader caching — Task 5 (`$loadedProjects` cache in worker.php).
- Test discovery via tree-sitter-php — Task 10.
- Sequential runner with filter — Tasks 11 + 13.
- Pretty TTY reporter — Task 12.
- End-to-end integration test — Task 9.

**Placeholder scan:** No "TBD", "implement later", or "handle edge cases" instructions. Every code block contains complete, runnable code. Known limitations (`expectException`, data providers) are called out **as limitations**, not as TODOs to satisfy in this plan.

**Type consistency:**
- `TestCase` (Task 6) → consumed by `discover_in_file` (Task 10) and `run` (Task 11). Field names match.
- `TestRequest` serializes `autoload`, `file`, `class`, `method` (Task 6) — matches `worker.php` JSON keys (Task 5).
- `TestOutcome` deserializes `class`, `method`, `status`, `message`, `trace`, `duration_ms` (Task 6) — matches what `worker.php` returns (Task 5).
- `TestStatus` variants `Pass`/`Fail`/`Error` deserialize from lowercase (`#[serde(rename_all = "lowercase")]`) — matches the `'pass'`/`'fail'`/`'error'` strings worker.php emits.

## Out-of-scope (deferred to follow-up plans)

- Full PHPUnit `TestRunner` integration (unblocks `expectException`, data providers, dependencies, listeners)
- Parallel execution (requires test-isolation reset strategy first)
- `@runInSeparateProcess` fallback path
- `phpunit.xml` config compat
- JUnit XML / TAP / TestDox reporters (CI integration)
- Coverage (PCOV/Xdebug + Clover/Cobertura output)
- Watch mode (filesystem-event-driven re-runs)
- `#[Test]` attribute discovery
- Custom assertion + extension hooks

Each becomes its own plan once this MVP is in place and the architecture is validated against a real-world suite.
