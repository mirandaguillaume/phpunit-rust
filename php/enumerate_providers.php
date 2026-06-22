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
    // Disable classmap-authoritative on any Composer ClassLoader (see
    // worker_fork.php for rationale).
    foreach (spl_autoload_functions() ?: [] as $fn) {
        if (is_array($fn) && isset($fn[0]) && is_object($fn[0])
            && $fn[0] instanceof \Composer\Autoload\ClassLoader
            && method_exists($fn[0], 'isClassMapAuthoritative')
            && $fn[0]->isClassMapAuthoritative()) {
            $fn[0]->setClassMapAuthoritative(false);
        }
    }
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

        // A class gated by an UNMET @requires / #[Requires*] will be SKIPPED, not
        // run. Return null (= "do not stride-split this method") so it stays a
        // single dispatch unit; the worker then emits exactly vanilla's skip
        // count (1 on PHPUnit >=10; one per row on 9.x). A split gated method
        // would emit one collapsed skip PER chunk on >=10 — over-counting by
        // chunks-1.
        if (\PhpunitRust\TestExecutor::classSkipReason($class) !== null) {
            return null;
        }

        // Method-level residual of the class gate: the requirement may sit on the
        // TEST method, not the class. We receive only (class, provider), so find
        // the test method(s) consuming this provider by reflection and, if ANY is
        // itself gated, return null. The choice is conservative on purpose: a
        // non-gated sibling sharing the provider merely loses its stride-split (a
        // little parallelism), but the gated method is kept whole so it emits
        // vanilla's single collapsed skip on >=10 instead of one-per-chunk.
        // Parity outranks the lost split.
        foreach (providerConsumers($class, $method) as $consumer) {
            if (\PhpunitRust\TestExecutor::methodSkipReason($class, $consumer) !== null) {
                return null;
            }
        }

        $reflMethod = new \ReflectionMethod($class, $method);
        if (!$reflMethod->isStatic()) {
            // Pre-10 style instance providers. We don't have a configured
            // test instance, so we can't safely invoke these. Fall back.
            return null;
        }

        $rows = $reflMethod->invoke(null);
        if (is_array($rows)) {
            $count = count($rows);
        } elseif ($rows instanceof \Traversable) {
            // Generators / iterators: materialize without preserving keys,
            // we only need the count.
            $count = iterator_count($rows);
        } else {
            $count = null;
        }
        // Tear the provider's returned objects down HERE, inside the try, so an
        // object with a throwing __destruct (e.g. php-parser's
        // NodeVisitorForTesting, which asserts in __destruct that its scripted
        // events were consumed — they never are during enumeration) is CAUGHT
        // below and degrades only THIS provider to null. Left to the frame/script
        // teardown the throw would be an uncatchable fatal (exit 255), and the
        // Rust side would then discard the row counts of EVERY provider in the
        // batch — disabling row-splitting for the whole suite.
        unset($rows);
        return $count;
    } catch (\Throwable) {
        return null;
    }
}

/**
 * Test methods on $class that consume the data provider $providerMethod, via
 * #[DataProvider('providerMethod')] or the legacy `@dataProvider providerMethod`
 * annotation. Only the SAME-class forms are matched — collect_provider_pairs
 * (Rust) never emits cross-class providers into the enumerate input, so the
 * consumer always lives on $class. Used to detect a method-level @requires gate
 * the (class, provider) pair alone can't reveal.
 *
 * @return list<string>
 */
function providerConsumers(string $class, string $providerMethod): array
{
    try {
        $ref = new \ReflectionClass($class);
    } catch (\Throwable) {
        return [];
    }
    $consumers = [];
    foreach ($ref->getMethods(\ReflectionMethod::IS_PUBLIC) as $m) {
        // PHPUnit 10+ attribute: #[DataProvider('providerMethod')].
        foreach ($m->getAttributes(\PHPUnit\Framework\Attributes\DataProvider::class) as $attr) {
            if ($attr->newInstance()->methodName() === $providerMethod) {
                $consumers[] = $m->getName();
                continue 2;
            }
        }
        // PHPUnit 9 / legacy PHPDoc: `@dataProvider providerMethod` (same-class
        // form, no `::`; cross-class providers aren't in the enumerate input).
        $doc = $m->getDocComment();
        if (is_string($doc) && preg_match_all('/@dataProvider\s+(\S+)/', $doc, $mm)) {
            foreach ($mm[1] as $name) {
                if ($name === $providerMethod) {
                    $consumers[] = $m->getName();
                    continue 2;
                }
            }
        }
    }
    return $consumers;
}
