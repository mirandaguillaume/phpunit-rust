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

require_once __DIR__ . '/vendor/autoload.php';

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

// ---------------------------------------------------------------------------
// 4. OPcache pre-warm: compile every test file before forking so children
//    inherit compiled opcodes via COW — no worker ever recompiles a file
//    the master already saw. Only runs when ext-opcache is loaded and
//    opcache.enable_cli is on (we pass -d opcache.enable_cli=1 from Rust).
// ---------------------------------------------------------------------------
// Pre-compile every test file into opcache so children inherit compiled
// opcodes via COW. We skip files that contain more than one top-level
// class because opcache_compile_file on PHP 8.1 (and likely later) leaks
// some class symbols into the master process — those symbols are then
// inherited by forks and trigger "class already in use" fatals when the
// child re-includes the file (observed on fakerphp/faker's BaseTest.php
// which declares both BaseTest and a Collection helper in the same file).
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
    foreach ($fileClassCount as $rp => $count) {
        if ($count === 1) {
            @opcache_compile_file($rp);
        }
    }
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
for ($i = 0; $i < $n; $i++) {
    $slotPid[$i] = $forkChildForSlot($i);
    if ($slotPid[$i] === -1) exit(1);
    $childPids[] = $slotPid[$i];
}

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
    &$slotPid, &$slotClosed, &$childPids, $forkChildForSlot
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
        // Any other exit: respawn for the next batch.
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
