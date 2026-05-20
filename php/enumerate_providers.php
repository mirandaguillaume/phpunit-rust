<?php

declare(strict_types=1);

/**
 * Provider row enumerator.
 *
 * CLI usage:
 *   php enumerate_providers.php \
 *     --autoload   /path/vendor/autoload.php \
 *     [--bootstrap /path/bootstrap.php]      \
 *     [--defines   '[[\"K\",\"V\"]]']
 *
 * Reads a JSON list of [className, providerMethod] pairs on stdin and
 * writes a JSON object to stdout mapping "className::providerMethod" to
 * the row count (int) or `null` if the provider couldn't be enumerated.
 *
 * Used by the Rust runner before forking workers: it learns exactly how
 * many rows each data-provider method produces so it can both schedule
 * heavy providers first (LPT) and split their rows across workers via
 * the existing TestExecutor row-filter ({chunk_index, total_chunks}).
 *
 * PHPUnit 10 requires data providers to be static, so calling them in
 * isolation (no test instance) is correct. Providers that depend on
 * runtime state we don't replicate here will throw and be reported as
 * `null` — the runner falls back to single-bucket dispatch for those.
 */

error_reporting(E_ALL & ~E_DEPRECATED);
@set_time_limit(0);

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

$autoload    = $args['autoload']  ?? null;
$bootstrap   = $args['bootstrap'] ?? null;
$definesJson = $args['defines']   ?? '[]';

if ($autoload === null) {
    fwrite(STDERR, "enumerate_providers.php: missing --autoload\n");
    exit(1);
}

$defines = json_decode($definesJson, true) ?? [];

// ---------------------------------------------------------------------------
// 2. Load project bootstrap (same layering as worker_fork.php)
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
} catch (\Throwable $e) {
    ob_end_clean();
    fwrite(STDERR, "enumerate_providers.php: autoload failed: " . $e->getMessage() . "\n");
    exit(1);
}
ob_end_clean();

if ($bootstrap !== null && is_file($bootstrap)) {
    ob_start();
    try {
        require_once $bootstrap;
    } catch (\Throwable $e) {
        ob_end_clean();
        // Bootstrap failures are non-fatal: many providers don't need it.
        fwrite(STDERR, "enumerate_providers.php: bootstrap failed (continuing): " . $e->getMessage() . "\n");
    }
    if (ob_get_level() > 0) { ob_end_clean(); }
}

// ---------------------------------------------------------------------------
// 3. Read provider list from stdin and enumerate row counts
// ---------------------------------------------------------------------------
$input = stream_get_contents(STDIN);
$pairs = json_decode($input ?: '[]', true);
if (!is_array($pairs)) {
    fwrite(STDERR, "enumerate_providers.php: stdin is not a JSON array\n");
    exit(1);
}

$out = [];
foreach ($pairs as $pair) {
    if (!is_array($pair) || count($pair) !== 2) continue;
    [$class, $method] = $pair;
    if (!is_string($class) || !is_string($method)) continue;

    $key = "$class::$method";
    $out[$key] = enumerateRows($class, $method);
}

echo json_encode($out), "\n";
exit(0);

/**
 * Call a static data-provider method and return its row count, or null
 * on any failure (class/method missing, exception thrown, return type
 * isn't iterable). All exceptions are swallowed — the runner treats
 * `null` as "fall back to single-bucket dispatch."
 */
function enumerateRows(string $class, string $method): ?int
{
    try {
        if (!class_exists($class)) return null;
        if (!method_exists($class, $method)) return null;

        $reflMethod = new \ReflectionMethod($class, $method);
        if (!$reflMethod->isStatic()) {
            // Pre-10 style instance providers. We don't have a configured
            // test instance, so we can't safely invoke these. Fall back.
            return null;
        }

        $rows = $reflMethod->invoke(null);
        if (is_array($rows)) {
            return count($rows);
        }
        if ($rows instanceof \Traversable) {
            // Generators / iterators: materialize without preserving keys,
            // we only need the count.
            return iterator_count($rows);
        }
        return null;
    } catch (\Throwable) {
        return null;
    }
}
