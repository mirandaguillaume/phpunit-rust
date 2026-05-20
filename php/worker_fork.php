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
 * Rust writes one BatchPlan JSON per child-stdin-fd; children stream
 * TestOutcome JSON lines back on child-stdout-fds, then exit naturally.
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
$childStdinFdsStr  = $args['child-stdin-fds']  ?? '';
$childStdoutFdsStr = $args['child-stdout-fds'] ?? '';

if ($autoload === null || $childStdinFdsStr === '' || $childStdoutFdsStr === '') {
    fwrite(STDERR, "worker_fork.php: missing --autoload, --child-stdin-fds, --child-stdout-fds\n");
    exit(1);
}

$defines        = json_decode($definesJson, true) ?? [];
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
// 5. Fork N children
// ---------------------------------------------------------------------------
$childPids = [];
for ($i = 0; $i < $n; $i++) {
    $pid = pcntl_fork();
    if ($pid === -1) {
        fwrite(STDERR, "worker_fork.php: pcntl_fork() failed\n");
        exit(1);
    }
    if ($pid === 0) {
        // Child process: close every sibling's streams.
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
// Child: read one BatchPlan, run all classes, stream TestOutcome JSON lines
// ---------------------------------------------------------------------------
function runChild($stdinStream, $stdoutStream): void
{
    // Read the entire batch plan. Rust closes the write end after writing,
    // so feof() fires naturally.
    $json = '';
    while (!feof($stdinStream)) {
        $chunk = fread($stdinStream, 65536);
        if ($chunk === false) break;
        $json .= $chunk;
    }
    fclose($stdinStream);

    $plan = json_decode(trim($json), true);
    if (!is_array($plan) || !isset($plan['classes'])) {
        fclose($stdoutStream);
        return;
    }

    foreach ($plan['classes'] as $entry) {
        $file    = (string) ($entry['file']    ?? '');
        $class   = (string) ($entry['class']   ?? '');
        $methods = (array)  ($entry['methods'] ?? []);

        if ($file === '' || $class === '') continue;

        if (!is_file($file)) {
            emitError($stdoutStream, $class, '<file>', "test file not found: $file");
            continue;
        }

        ob_start();
        try {
            require_once $file;
            if (!class_exists($class)) {
                ob_end_clean();
                emitError($stdoutStream, $class, '<class>',
                    "class $class not found after loading $file");
                continue;
            }
            $outcomes = TestExecutor::runClass($class, $methods, null);
        } catch (\Throwable $e) {
            while (ob_get_level() > 0) ob_end_clean();
            emitError($stdoutStream, $class, '<class>',
                'exception: ' . $e->getMessage(), $e->getTraceAsString());
            continue;
        }
        ob_end_clean();

        foreach ($outcomes as $outcome) {
            fwrite($stdoutStream, json_encode($outcome) . "\n");
            fflush($stdoutStream);
        }
    }

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
