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
$childStdinFdsStr  = $args['child-stdin-fds']  ?? '';
$childStdoutFdsStr = $args['child-stdout-fds'] ?? '';

if ($autoload === null || $childStdinFdsStr === '' || $childStdoutFdsStr === '') {
    fwrite(STDERR, "worker_fork.php: missing --autoload, --child-stdin-fds, --child-stdout-fds\n");
    exit(1);
}

$defines        = json_decode($definesJson, true) ?? [];
$envVars        = json_decode($envJson,     true) ?? [];
$serverVars     = json_decode($serverJson,  true) ?? [];
$iniVars        = json_decode($iniJson,     true) ?? [];

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
// 4. Open PHP stream handles for all child FDs BEFORE fork so children inherit
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
// 5. Install master-side signal handlers
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
        @posix_kill($pid, SIGKILL);
    }
    exit(128 + $sig);
};
pcntl_signal(SIGTERM, $signalHandler);
pcntl_signal(SIGINT,  $signalHandler);
pcntl_signal(SIGHUP,  $signalHandler);

// ---------------------------------------------------------------------------
// 6. Fork N children
// ---------------------------------------------------------------------------
for ($i = 0; $i < $n; $i++) {
    $pid = pcntl_fork();
    if ($pid === -1) {
        fwrite(STDERR, "worker_fork.php: pcntl_fork() failed\n");
        exit(1);
    }
    if ($pid === 0) {
        // Child process: restore default signal disposition so SIGTERM/SIGINT
        // terminate this worker immediately instead of running the master's
        // handler with a stale (pre-fork) $childPids snapshot.
        pcntl_signal(SIGTERM, SIG_DFL);
        pcntl_signal(SIGINT,  SIG_DFL);
        pcntl_signal(SIGHUP,  SIG_DFL);
        // Close every sibling's streams.
        // CRITICAL: each child MUST close the write ends it does not own;
        // Rust's reader on those pipes blocks until the last writer exits.
        for ($j = 0; $j < $n; $j++) {
            if ($j !== $i) {
                fclose($childStdinStreams[$j]);
                fclose($childStdoutStreams[$j]);
            }
        }
        runChild($childStdinStreams[$i], $childStdoutStreams[$i]);
        exit(0);
    }
    $childPids[] = $pid;
}

// Master: close all child FDs, then wait for children.
for ($i = 0; $i < $n; $i++) {
    fclose($childStdinStreams[$i]);
    fclose($childStdoutStreams[$i]);
}
foreach ($childPids as $pid) {
    pcntl_waitpid($pid, $status);
}
exit(0);

// ---------------------------------------------------------------------------
// Child: read newline-delimited BatchPlans in a loop, run classes per batch,
// stream TestOutcome JSON lines back and emit {"batch_done": true} between
// batches as a ready signal. Exit cleanly when Rust closes our stdin.
// ---------------------------------------------------------------------------
function runChild($stdinStream, $stdoutStream): void
{
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

    while (true) {
        $line = fgets($stdinStream);
        if ($line === false || $line === '') break;  // EOF: clean shutdown
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
                require_once $file;
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
