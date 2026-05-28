<?php

declare(strict_types=1);

/**
 * Fork-based batch worker master.
 *
 * CLI usage:
 *   php worker_fork.php \
 *     --autoload   /path/vendor/autoload.php \
 *     [--bootstrap /path/bootstrap.php]      \
 *     [--defines   '[[\"K\",\"V\"]]']        \
 *     --child-stdin-fds  7,9,11,13           \
 *     --child-stdout-fds 8,10,12,14
 *
 * Rust streams newline-delimited BatchPlan JSONs per child-stdin-fd
 * (work-stealing pool); children stream TestOutcome JSON lines back
 * and emit {"batch_done": true} after each batch as a ready signal.
 * Children exit when Rust closes their stdin (EOF).
 *
 * Requires: pcntl extension (standard PHP CLI on Linux/macOS).
 */

error_reporting(E_ALL & ~E_DEPRECATED);
@set_time_limit(0);

// POC instrumentation: write phase timings to STDERR. Turn on with
// PHPUNIT_RUST_TIMING=1 in the env. Output format:
//   [TIMING] phase=name delta_ms=X total_ms=Y
$__t0 = microtime(true);
$__tprev = $__t0;
$__timing_enabled = getenv('PHPUNIT_RUST_TIMING') === '1';
$__log_phase = function(string $name) use (&$__tprev, $__t0, $__timing_enabled): void {
    if (!$__timing_enabled) return;
    $now = microtime(true);
    fwrite(STDERR, sprintf("[TIMING] phase=%-22s delta_ms=%6.1f total_ms=%6.1f\n",
        $name, ($now - $__tprev) * 1000, ($now - $__t0) * 1000));
    $__tprev = $now;
};
$__log_phase('start');

require_once __DIR__ . '/vendor/autoload.php';
$__log_phase('worker_vendor_autoload');

use PhpunitRust\TestExecutor;

// ---------------------------------------------------------------------------
// 1. Parse CLI args
// ---------------------------------------------------------------------------
$args = [];
for ($i = 1; $i < $argc; $i++) {
    if (str_starts_with($argv[$i], '--') && isset($argv[$i + 1])) {
        $key = substr($argv[$i], 2);
        $args[$key] = $argv[++$i];
    }
}

$autoload        = $args['autoload']           ?? null;
$bootstrap       = $args['bootstrap']          ?? null;
$definesJson     = $args['defines']            ?? '[]';
$envJson         = $args['env']                ?? '[]';
$serverJson      = $args['server']             ?? '[]';
$iniJson         = $args['ini']                ?? '[]';
$childStdinFdsStr  = $args['child-stdin-fds']    ?? '';
$childStdoutFdsStr = $args['child-stdout-fds']   ?? '';
$classMapFile      = $args['class-map-file']     ?? null;
$workerMemoryLimit = $args['worker-memory-limit'] ?? '512M';
$maxBatches        = (int) ($args['max-batches-per-child'] ?? '0'); // 0 = unlimited

if ($autoload === null || $childStdinFdsStr === '' || $childStdoutFdsStr === '') {
    fwrite(STDERR, "worker_fork.php: missing --autoload, --child-stdin-fds, --child-stdout-fds\n");
    exit(1);
}

$defines       = json_decode($definesJson, true) ?? [];
$envVars       = json_decode($envJson,     true) ?? [];
$serverVars    = json_decode($serverJson,  true) ?? [];
$iniVars       = json_decode($iniJson,     true) ?? [];
$classMapExtra = ($classMapFile !== null && is_file($classMapFile))
    ? (json_decode(file_get_contents($classMapFile), true) ?? [])
    : [];

// Apply <ini> first so error_reporting / memory_limit etc. are in effect
// before we run any user code in autoload/bootstrap.
foreach ($iniVars as $pair) {
    if (is_array($pair) && count($pair) === 2 && is_string($pair[0])) {
        @ini_set($pair[0], (string) $pair[1]);
    }
}
// Apply <env>: each is [name, value, force]. PHPUnit's force=false means
// "don't clobber a value already in the shell environment".
foreach ($envVars as $entry) {
    if (!is_array($entry) || count($entry) !== 3) continue;
    [$name, $value, $force] = $entry;
    if (!is_string($name)) continue;
    if (!$force && getenv($name) !== false) continue;
    putenv("$name=$value");
    $_ENV[$name] = $value;
}
// Apply <server>: populate $_SERVER (commonly used for HTTPS, SCRIPT_NAME,
// etc.). PHPUnit does NOT honour `force` on <server> in any version
// I can find, so we always set.
foreach ($serverVars as $pair) {
    if (is_array($pair) && count($pair) === 2 && is_string($pair[0])) {
        $_SERVER[$pair[0]] = $pair[1];
    }
}
$childStdinFds  = array_map('intval', explode(',', $childStdinFdsStr));
$childStdoutFds = array_map('intval', explode(',', $childStdoutFdsStr));
$n              = count($childStdinFds);

if ($n !== count($childStdoutFds) || $n < 1) {
    fwrite(STDERR, "worker_fork.php: stdin/stdout fd count mismatch or zero\n");
    exit(1);
}

// ---------------------------------------------------------------------------
// 2. Layer 1: autoloader
// ---------------------------------------------------------------------------
ob_start();
try {
    require_once $autoload;
    // Register a secondary autoloader for test classes that Composer's
    // classmap doesn't cover (e.g. test helpers whose providers call sibling
    // test classes). The map is built from the runner's discovery index.
    if (!empty($classMapExtra)) {
        spl_autoload_register(static function (string $class) use ($classMapExtra): void {
            if (isset($classMapExtra[$class]) && is_file($classMapExtra[$class])) {
                require_once $classMapExtra[$class];
            }
        });
    }
    foreach ($defines as $pair) {
        if (is_array($pair) && count($pair) === 2
            && is_string($pair[0]) && !defined($pair[0])) {
            define($pair[0], $pair[1]);
        }
    }
    // PHPUnit 10+ Configuration registry initialisation.
    if (class_exists(\PHPUnit\TextUI\Configuration\Registry::class)) {
        try {
            \PHPUnit\TextUI\Configuration\Registry::init(
                (new \PHPUnit\TextUI\CliArguments\Builder)->fromParameters([]),
                \PHPUnit\TextUI\XmlConfiguration\DefaultConfiguration::create(),
            );
        } catch (\Throwable) { /* version drift; non-fatal */ }
    }
} catch (\Throwable $e) {
    ob_end_clean();
    fwrite(STDERR, "worker_fork.php: autoload failed: " . $e->getMessage() . "\n");
    exit(1);
}
ob_end_clean();
$__log_phase('project_autoload');

// ---------------------------------------------------------------------------
// 3. Layer 2: bootstrap (optional)
// ---------------------------------------------------------------------------
if ($bootstrap !== null && is_file($bootstrap)) {
    ob_start();
    try {
        require_once $bootstrap;
    } catch (\Throwable $e) {
        ob_end_clean();
        fwrite(STDERR, "worker_fork.php: bootstrap failed: " . $e->getMessage() . "\n");
        exit(1);
    }
    ob_end_clean();
}
$__log_phase('bootstrap');

// ---------------------------------------------------------------------------
// 4. OPcache pre-warm: compile every test file before forking so children
//    inherit compiled opcodes via COW — no worker ever recompiles a file
//    the master already saw. Only runs when ext-opcache is loaded and
//    opcache.enable_cli is on (we pass -d opcache.enable_cli=1 from Rust).
// ---------------------------------------------------------------------------
// Pre-compile every test file into opcache so children inherit compiled
// opcodes via COW. We skip files that declare more than one top-level
// symbol or any top-level function because opcache_compile_file on
// PHP 8.1 (and likely later) leaks the secondary symbols into the master
// process — those symbols are then inherited by forks and trigger
// "Cannot declare ..." fatals when the child re-includes the file.
// Observed cases:
//   - fakerphp/faker BaseTest.php — class BaseTest + class Collection
//   - guzzlehttp/psr7 StreamTest.php — class StreamTest + namespaced
//     function GuzzleHttp\Psr7\fread() that shadows the builtin
if (!empty($classMapExtra)
    && function_exists('opcache_compile_file')
    && filter_var(ini_get('opcache.enable_cli'), FILTER_VALIDATE_BOOLEAN)) {
    $fileClassCount = [];
    foreach ($classMapExtra as $file) {
        if (is_string($file) && is_file($file)) {
            $rp = realpath($file);
            if ($rp !== false) {
                $fileClassCount[$rp] = ($fileClassCount[$rp] ?? 0) + 1;
            }
        }
    }
    // Skip the pre-warm loop entirely on small suites: opcache_compile_file
    // costs ~0.5ms per file (measured: 50 files ≈ 25ms), but the COW saving
    // it buys per worker is roughly the same as a plain require_once on the
    // first batch — i.e. negligible when there are few files. Below the
    // threshold, lazy require_once in the child wins on cold runs by ~20ms.
    // Above the threshold, sharing compiled opcodes via COW dominates.
    // Threshold overridable via PHPUNIT_RUST_OPCACHE_THRESHOLD for A/B
    // benchmarking; 0 forces always-prewarm, a huge value forces never.
    $opcacheThreshold = (int) (getenv('PHPUNIT_RUST_OPCACHE_THRESHOLD') ?: '50');
    if (count($fileClassCount) >= $opcacheThreshold) {
        $dbgPath = getenv('PHPUNIT_RUST_OPCACHE_DEBUG') === '1' ? '/tmp/opcache_dbg.log' : null;
        if ($dbgPath) @file_put_contents($dbgPath, "");
        foreach ($fileClassCount as $rp => $count) {
            if ($count !== 1) continue;          // multi-class file → skip
            if (file_has_top_level_function($rp)) continue;  // file with fn → skip
            // Skip fixture/data PHP files: rector ships ~1500 such files
            // under rules-tests/**/Fixture/, phpstan under
            // tests/PHPStan/**/data/, doctrine under tests/Fixtures/.
            // They register classes the discovery walker picked up via
            // class_file_index, but no test ever require_once's them
            // directly — they're loaded by code-rewriting fixtures or
            // psalm-style include-pair tests. Pre-warming them only adds
            // master compile time and burns memory we don't recover.
            if (preg_match('#/(Fixture|Fixtures|fixtures|_fixtures|_files|data)/#', $rp)) continue;
            if ($dbgPath) @file_put_contents($dbgPath,
                sprintf("[%.3f] %s\n", microtime(true), $rp), FILE_APPEND);
            @opcache_compile_file($rp);
        }
    }
}
$__log_phase('opcache_prewarm');

/**
 * Return true if the PHP file declares at least one top-level (depth-0)
 * function. Used to skip opcache pre-warm for files that mix a test class
 * with namespaced helper functions, because opcache_compile_file leaks the
 * function into the master and causes redeclaration fatals in forks.
 *
 * Brace-depth tracking is approximate but correct for well-formed PHP:
 * anonymous classes and closures have their `function` / `class` token at
 * depth 0 only when they're the file's top-level declaration; nested ones
 * sit inside another body so depth > 0 and aren't counted.
 */
function file_has_top_level_function(string $file): bool
{
    $src = @file_get_contents($file);
    if ($src === false) return true; // err on the safe side — skip pre-warm
    $tokens = @token_get_all($src);
    if (!is_array($tokens) || empty($tokens)) return false;
    $depth = 0;
    foreach ($tokens as $tok) {
        if (is_array($tok)) {
            if ($depth === 0 && $tok[0] === T_FUNCTION) {
                return true;
            }
        } else {
            if ($tok === '{') $depth++;
            elseif ($tok === '}') $depth--;
        }
    }
    return false;
}

// ---------------------------------------------------------------------------
// 6. Open PHP stream handles for all child FDs BEFORE fork so children inherit
// ---------------------------------------------------------------------------
$childStdinStreams  = [];
$childStdoutStreams = [];
for ($i = 0; $i < $n; $i++) {
    $childStdinStreams[$i]  = fopen('php://fd/' . $childStdinFds[$i],  'r');
    $childStdoutStreams[$i] = fopen('php://fd/' . $childStdoutFds[$i], 'w');
    if ($childStdinStreams[$i] === false || $childStdoutStreams[$i] === false) {
        fwrite(STDERR, "worker_fork.php: failed to open fd for slot $i\n");
        exit(1);
    }
}

// ---------------------------------------------------------------------------
// 7. Install master-side signal handlers
// ---------------------------------------------------------------------------
// Triggered by:
//   - Rust's Drop sending SIGTERM during normal shutdown
//   - The kernel's PR_SET_PDEATHSIG firing SIGTERM when Rust dies of any
//     other cause (SIGKILL, panic before Drop, OOM, …)
//   - User hitting Ctrl-C on phpunit-rust (SIGINT propagates to the
//     process group)
// We SIGKILL every forked child immediately so a child stuck in setUp or
// an infinite-loop test can't outlive its parent.
$childPids = [];
pcntl_async_signals(true);
$signalHandler = function (int $sig) use (&$childPids): void {
    foreach ($childPids as $pid) {
        @posix_kill(-$pid, SIGKILL);  // negative PID = kill entire process group
    }
    exit(128 + $sig);
};
pcntl_signal(SIGTERM, $signalHandler);
pcntl_signal(SIGINT,  $signalHandler);
pcntl_signal(SIGHUP,  $signalHandler);

// ---------------------------------------------------------------------------
// 8. Fork N children. When $maxBatches > 0 we behave as a fork-server: a
//    child exits voluntarily after processing $maxBatches batches, and the
//    master forks a fresh replacement that inherits the master's warm state
//    (autoload + bootstrap) via COW. This caps per-fork state accumulation —
//    e.g. Symfony bridge deprecation collectors — without paying the cost
//    of a one-batch-per-process model.
//    When $maxBatches = 0 we keep the original long-lived behaviour.
// ---------------------------------------------------------------------------
$forkChildForSlot = static function (int $slot) use (
    $childStdinStreams, $childStdoutStreams, $n, $workerMemoryLimit, $maxBatches
): int {
    $pid = pcntl_fork();
    if ($pid === -1) {
        fwrite(STDERR, "worker_fork.php: pcntl_fork() failed for slot $slot\n");
        return -1;
    }
    if ($pid === 0) {
        posix_setpgid(0, 0);
        pcntl_signal(SIGTERM, SIG_DFL);
        pcntl_signal(SIGINT,  SIG_DFL);
        pcntl_signal(SIGHUP,  SIG_DFL);
        for ($j = 0; $j < $n; $j++) {
            if ($j !== $slot) {
                @fclose($childStdinStreams[$j]);
                @fclose($childStdoutStreams[$j]);
            }
        }
        runChild($childStdinStreams[$slot], $childStdoutStreams[$slot], $workerMemoryLimit, $maxBatches);
        exit(0);
    }
    @posix_setpgid($pid, $pid);
    return $pid;
};

$slotPid    = array_fill(0, $n, 0);
$slotClosed = array_fill(0, $n, false);
$__log_phase('pre_fork');
for ($i = 0; $i < $n; $i++) {
    $slotPid[$i] = $forkChildForSlot($i);
    if ($slotPid[$i] === -1) exit(1);
    $childPids[] = $slotPid[$i];
}
$__log_phase('post_fork_all_children');

if ($maxBatches === 0) {
    // Long-lived mode: just wait for all children to exit (on Rust closing
    // their stdin) — no respawning. Closing master FDs here lets Rust's
    // reader see EOF when each child eventually exits.
    for ($i = 0; $i < $n; $i++) {
        fclose($childStdinStreams[$i]);
        fclose($childStdoutStreams[$i]);
    }
    foreach ($childPids as $pid) {
        pcntl_waitpid($pid, $status);
    }
    exit(0);
}

// Fork-server mode: install SIGCHLD handler to respawn children that exit
// voluntarily (after $maxBatches). A child exit with code 7 means "EOF on
// stdin, slot is closed" — we don't respawn then.
//
// CRITICAL: keep $childStdinStreams / $childStdoutStreams open in the master
// across the entire run. The kernel-level FDs underlie them; fresh forked
// children inherit those FDs only if the master still holds them.
pcntl_signal(SIGCHLD, function () use (
    &$slotPid, &$slotClosed, &$childPids, &$childStdoutStreams, $forkChildForSlot
): void {
    while (($deadPid = pcntl_waitpid(-1, $status, WNOHANG)) > 0) {
        $slot = array_search($deadPid, $slotPid, true);
        if ($slot === false) continue;
        $childPids = array_values(array_diff($childPids, [$deadPid]));
        // exit code 7 = child saw EOF on stdin; Rust closed this slot.
        if (pcntl_wifexited($status) && pcntl_wexitstatus($status) === 7) {
            $slotClosed[$slot] = true;
            $slotPid[$slot] = 0;
            continue;
        }
        // Distinguish a CRASH (non-zero exit, killed by signal) from a clean
        // recycle (exit 0 — K-batches limit or force_exit_after). Only crashes
        // need a slot_died notice: a clean recycle has already emitted
        // batch_done so Rust's in_flight[slot] is None and the previous batch
        // is fully accounted for. Sending slot_died here would race with the
        // NEXT batch's dispatch and make Rust attribute the death to the new
        // in-flight plan → synthetic errors for tests that ran fine.
        $exitedClean    = pcntl_wifexited($status) && pcntl_wexitstatus($status) === 0;
        $isCrash        = !$exitedClean;
        if ($isCrash
            && isset($childStdoutStreams[$slot])
            && is_resource($childStdoutStreams[$slot])) {
            $exitCode = pcntl_wifexited($status)   ? pcntl_wexitstatus($status) : -1;
            $signal   = pcntl_wifsignaled($status) ? pcntl_wtermsig($status)    :  0;
            @fwrite($childStdoutStreams[$slot], json_encode([
                'slot_died' => true,
                'exit_code' => $exitCode,
                'signal'    => $signal,
            ]) . "\n");
            @fflush($childStdoutStreams[$slot]);
        }
        $newPid = $forkChildForSlot($slot);
        if ($newPid === -1) {
            $slotClosed[$slot] = true;
            $slotPid[$slot] = 0;
            continue;
        }
        $slotPid[$slot] = $newPid;
        $childPids[]    = $newPid;
    }
});

while (in_array(false, $slotClosed, true)) {
    pcntl_signal_dispatch();
    usleep(5_000);
}
foreach ($childPids as $pid) {
    @pcntl_waitpid($pid, $status, WNOHANG);
}
exit(0);

// ---------------------------------------------------------------------------
// Child: read newline-delimited BatchPlans in a loop, run classes per batch,
// stream TestOutcome JSON lines back and emit {"batch_done": true} between
// batches as a ready signal. Exit cleanly when Rust closes our stdin.
// ---------------------------------------------------------------------------
function runChild($stdinStream, $stdoutStream, string $memoryLimit, int $maxBatches = 0): void
{
    // Apply the worker-specific memory limit. This intentionally overrides
    // phpunit.xml's <ini name="memory_limit"> because that setting is designed
    // for short single-run PHP invocations, not long-lived workers that execute
    // thousands of tests in sequence. The value is controlled by --worker-memory-limit
    // (default "512M"); pass "-1" to restore unlimited behaviour.
    @ini_set('memory_limit', $memoryLimit);

    // Current-batch state, captured by reference in the shutdown handler.
    // We reset both to empty after each clean batch so the handler doesn't
    // double-emit; it only fires for uncatchable fatals (E_COMPILE_ERROR etc.)
    // mid-batch, where these point to the class that killed us and the
    // remainder of its batch that we never reached.
    $currentClasses = [];
    $nextIdx        = 0;

    register_shutdown_function(function() use (&$currentClasses, &$nextIdx, $stdoutStream): void {
        while (ob_get_level() > 0) @ob_end_clean();
        for ($i = $nextIdx; $i < count($currentClasses); $i++) {
            $class = (string)($currentClasses[$i]['class'] ?? '');
            if ($class === '') continue;
            @fwrite($stdoutStream, json_encode([
                'class'       => $class,
                'method'      => '<class>',
                'dataset'     => null,
                'status'      => 'error',
                'message'     => 'worker process terminated before this class could run',
                'trace'       => null,
                'duration_ms' => 0.0,
            ]) . "\n");
            @fflush($stdoutStream);
        }
    });

    $batchesProcessed = 0;
    while (true) {
        $line = fgets($stdinStream);
        if ($line === false || $line === '') {
            // EOF on stdin: Rust closed the pipe. Tell master via exit(7)
            // so it knows this slot is closed (don't respawn).
            exit(7);
        }
        $line = trim($line);
        if ($line === '') continue;

        $plan = json_decode($line, true);
        if (!is_array($plan) || !isset($plan['classes'])) {
            // Malformed line: still ack so master doesn't deadlock on us.
            fwrite($stdoutStream, json_encode(['batch_done' => true]) . "\n");
            fflush($stdoutStream);
            continue;
        }

        $currentClasses = $plan['classes'];
        $nextIdx        = 0;

        foreach ($currentClasses as $i => $entry) {
            $file       = (string) ($entry['file']    ?? '');
            $class      = (string) ($entry['class']   ?? '');
            $methods    = (array)  ($entry['methods'] ?? []);
            // Optional stride row filter, e.g. {chunk_index: 2, total_chunks: 4}.
            // Applied uniformly to every data-provider method in this batch:
            // the runner emits one BatchClass per chunk when it wants to split
            // a heavy provider across workers.
            $rowFilter  = $entry['row_filter'] ?? null;
            if (!is_array($rowFilter)) { $rowFilter = null; }

            // Mark this class as "in progress" before touching it.
            // If exit()/fatal fires here, shutdown reports from $i onward.
            $nextIdx = $i;

            if ($file === '' || $class === '') {
                $nextIdx = $i + 1;
                continue;
            }

            if (!is_file($file)) {
                emitError($stdoutStream, $class, '<file>', "test file not found: $file");
                $nextIdx = $i + 1;
                continue;
            }

            ob_start();
            try {
                // Guard against E_COMPILE_ERROR "Cannot redeclare class": when
                // multiple fixture files share the same FQCN (e.g. PHPUnit's own
                // end-to-end fixtures), loading the second file in the same
                // long-lived worker process would be fatal. Skip require_once
                // when the class is already defined from a previous batch.
                if (!class_exists($class, false)) {
                    require_once $file;
                } else {
                    // Class already loaded from a different file. Emit errors for
                    // any methods in this batch that don't exist on the loaded
                    // definition so they appear in the report rather than being
                    // silently dropped by MethodPlanner's hasMethod filter.
                    $conflictMethods = array_unique(array_filter(
                        $methods, fn($m) => !method_exists($class, $m)
                    ));
                    foreach ($conflictMethods as $m) {
                        emitError($stdoutStream, $class, $m,
                            "method $m not found on $class (FQCN defined in multiple files; " .
                            "loaded from a different path — run with more workers to avoid conflicts)");
                    }
                    $methods = array_values(array_filter(
                        $methods, fn($m) => method_exists($class, $m)
                    ));
                    if (empty($methods)) {
                        ob_end_clean();
                        $nextIdx = $i + 1;
                        continue;
                    }
                }
                foreach ($entry['required_files'] ?? [] as $rf) {
                    if (is_string($rf) && is_file($rf)) {
                        require_once $rf;
                    }
                }
                if (!class_exists($class)) {
                    ob_end_clean();
                    emitError($stdoutStream, $class, '<class>',
                        "class $class not found after loading $file");
                    $nextIdx = $i + 1;
                    continue;
                }
                $outcomes = TestExecutor::runClass($class, $methods, $rowFilter);
            } catch (\Throwable $e) {
                while (ob_get_level() > 0) ob_end_clean();
                emitError($stdoutStream, $class, '<class>',
                    'exception: ' . $e->getMessage(), $e->getTraceAsString());
                $nextIdx = $i + 1;
                continue;
            }
            ob_end_clean();

            foreach ($outcomes as $outcome) {
                fwrite($stdoutStream, json_encode($outcome) . "\n");
                fflush($stdoutStream);
            }
            $nextIdx = $i + 1;
        }

        // Batch done cleanly: clear state so shutdown handler is a no-op
        // if the process exits naturally on the next read, then signal ready.
        $currentClasses = [];
        $nextIdx        = 0;
        fwrite($stdoutStream, json_encode(['batch_done' => true]) . "\n");
        fflush($stdoutStream);

        // Fork-server mode: exit voluntarily after $maxBatches batches so
        // the master can fork a fresh child for the next batch. This bounds
        // per-fork state accumulation (e.g. Symfony bridge collectors).
        //
        // Per-batch override: when the runner marks a batch with
        // `force_exit_after` (set on batches whose class is is_stateful —
        // it registers stream wrappers, error handlers, etc.), we exit
        // immediately regardless of $batchesProcessed. The master forks
        // a fresh child for the next batch, guaranteeing zero cross-batch
        // pollution from this class's global side effects.
        // PHPUNIT_RUST_NO_ISOLATION=1 disables the per-batch fresh-fork
        // for stateful classes — used only for A/B benchmarks to quantify
        // the parity-vs-perf trade-off.
        if (!empty($plan['force_exit_after']) && !getenv('PHPUNIT_RUST_NO_ISOLATION')) {
            exit(0);
        }
        if ($maxBatches > 0) {
            $batchesProcessed++;
            if ($batchesProcessed >= $maxBatches) {
                exit(0);
            }
        }
    }

    fclose($stdinStream);
    fclose($stdoutStream);
}

function emitError($stream, string $class, string $method,
                   string $msg, ?string $trace = null): void
{
    fwrite($stream, json_encode([
        'class'       => $class,
        'method'      => $method,
        'dataset'     => null,
        'status'      => 'error',
        'message'     => $msg,
        'trace'       => $trace,
        'duration_ms' => 0.0,
    ]) . "\n");
    fflush($stream);
}
