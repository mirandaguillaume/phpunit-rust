<?php

declare(strict_types=1);

namespace Proust;

use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\Attributes\DataProviderExternal;
use PHPUnit\Framework\Attributes\Depends;
use PHPUnit\Framework\Attributes\TestWith;
use PHPUnit\Framework\Attributes\TestWithJson;

/**
 * Turns "run these methods of this class" into an ordered sequence of steps.
 * Each step is one invocation: a method, optional dataset key, optional args
 * from a data provider, and the list of dependencies whose return values
 * should be prepended to args at invocation time.
 *
 * @phpstan-type Step array{method:string,dataset:?string,args:list<mixed>,args_hash:?string,is_duplicate:bool,depends:list<string>}
 */
final class MethodPlanner
{
    /**
     * @param class-string $class
     * @param list<string> $methods Empty = all `test*` methods on the class.
     * @return list<Step>
     */
    public static function plan(string $class, array $methods, ?array $rowFilter = null): array
    {
        $ref = new \ReflectionClass($class);
        $candidate = empty($methods) ? self::allTestMethods($ref) : $methods;

        // Filter to methods that actually exist on the loaded class.
        // Defends against FQCN-conflict batches in long-lived workers where
        // the same FQCN is defined in multiple files but only the first file's
        // definition was loaded (PHP cannot redefine a class in one process).
        $candidate = array_values(array_filter($candidate, [$ref, 'hasMethod']));

        // Resolve dependencies and topologically sort.
        $ordered = self::topoSort($ref, $candidate);

        $steps = [];
        foreach ($ordered as $methodName) {
            $methodRef = $ref->getMethod($methodName);
            $depends   = self::dependsOf($methodRef);
            try {
                $datasets = self::dataSetsFor($ref, $methodRef);
            } catch (\Throwable $e) {
                // Provider threw (e.g. a cross-class dependency not loaded in this
                // worker slot). Emit a single error step so the method appears in
                // the report instead of silently crashing the entire class.
                $steps[] = [
                    'method'         => $methodName,
                    'dataset'        => null,
                    'args'           => [],
                    'args_hash'      => null,
                    'is_duplicate'   => false,
                    'depends'        => [],
                    'provider_error' => $e->getMessage(),
                ];
                continue;
            }
            if ($datasets === null) {
                // Non-parameterized: one step. row_filter has no effect.
                $steps[] = [
                    'method'       => $methodName,
                    'dataset'      => null,
                    'args'         => [],
                    'args_hash'    => null,
                    'is_duplicate' => false,
                    'depends'      => $depends,
                ];
                continue;
            }
            // Materialize datasets once so we can index/slice them.
            $datasets = is_array($datasets) ? $datasets : iterator_to_array($datasets);
            // An empty data provider is NOT zero tests: PHPUnit reports the
            // method as a single SKIPPED test ("skipped by data provider"). Emit
            // one skip-step so the method still appears in the count — otherwise
            // the method vanishes. (faker's localeDataProvider returns [] when no
            // locale dirs are present, which made rust undercount faker by the
            // number of such methods.)
            if (count($datasets) === 0) {
                $steps[] = [
                    'method'         => $methodName,
                    'dataset'        => null,
                    'args'           => [],
                    'args_hash'      => null,
                    'is_duplicate'   => false,
                    'depends'        => $depends,
                    'empty_provider' => true,
                ];
                continue;
            }
            // Apply row_filter if present (only on this method's rows).
            // The filter is `{chunk_index, total_chunks}`. We keep rows
            // whose 0-based position satisfies `pos % total_chunks == chunk_index`
            // (stride splitting — keeps related rows together for typical
            // provider orderings while balancing across workers).
            $kept = $datasets;
            if ($rowFilter !== null
                && isset($rowFilter['chunk_index'], $rowFilter['total_chunks'])
                && is_int($rowFilter['chunk_index'])
                && is_int($rowFilter['total_chunks'])
                && $rowFilter['total_chunks'] > 1) {
                $chunkIndex  = $rowFilter['chunk_index'];
                $totalChunks = $rowFilter['total_chunks'];
                $kept = [];
                $pos = 0;
                foreach ($datasets as $key => $row) {
                    if ($pos % $totalChunks === $chunkIndex) {
                        $kept[$key] = $row;
                    }
                    $pos++;
                }
            }
            $seenHashes = [];
            foreach ($kept as $key => $row) {
                $rowData     = is_array($row) ? $row : iterator_to_array($row);
                // Hash the row for duplicate detection, guarded by two rules:
                //  1. Object-bearing rows are NEVER deduplicated. json_encode only
                //     sees PUBLIC properties, so two distinct provider objects that
                //     differ only in PRIVATE state (e.g. php-parser's
                //     NodeVisitorForTesting, whose scripted returns are private)
                //     hash identically — the 2nd row would be wrongly memoized,
                //     never executed, leaving its object unconsumed, and a throwing
                //     __destruct on it then collapses the entire class.
                //  2. Some provider values have side-effecting magic methods (e.g.
                //     Carbon\CarbonPeriod endless throws in jsonSerialize()); if
                //     hashing throws we must NOT take down the class.
                // In both cases the row is treated as unique — exactly as vanilla
                // PHPUnit, which runs every data row.
                if (self::rowContainsObject($rowData)) {
                    $rowHash = null;
                } else {
                    try {
                        $encoded = json_encode(array_values($rowData));
                        $rowHash = md5($encoded !== false ? $encoded : serialize(array_values($rowData)));
                    } catch (\Throwable $e) {
                        $rowHash = null;
                    }
                }
                $isDuplicate = $rowHash !== null && isset($seenHashes[$rowHash]);
                if ($rowHash !== null) {
                    $seenHashes[$rowHash] = true;
                }
                $steps[] = [
                    'method'       => $methodName,
                    'dataset'      => is_int($key) ? "#{$key}" : (string) $key,
                    'args'         => array_values($rowData),
                    'args_hash'    => $rowHash,
                    'is_duplicate' => $isDuplicate,
                    'depends'      => $depends,
                ];
            }
        }
        return $steps;
    }

    /**
     * True if a data-provider row holds an object anywhere — top level or nested
     * inside arrays. Such rows must never be deduplicated: json_encode sees only
     * public state, so distinct objects differing only in private state hash
     * alike, and the collision wrongly memoizes (skips) a row that PHPUnit runs.
     *
     * @param array<mixed> $row
     */
    private static function rowContainsObject(array $row): bool
    {
        foreach ($row as $v) {
            if (is_object($v)) {
                return true;
            }
            if (is_array($v) && self::rowContainsObject($v)) {
                return true;
            }
        }
        return false;
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
    public static function dependsOf(\ReflectionMethod $m): array
    {
        $out = [];
        // PHPUnit 10+ attribute style.
        foreach ($m->getAttributes(Depends::class) as $attr) {
            $instance = $attr->newInstance();
            $out[] = $instance->methodName();
        }
        // PHPUnit 9 / legacy PHPDoc style: `@depends methodName`.
        // Many PHPUnit 10 codebases still use this for compat, and PHPUnit 9
        // has no attributes at all.
        $doc = $m->getDocComment();
        if (is_string($doc) && preg_match_all('/@depends\s+(\S+)/', $doc, $matches)) {
            foreach ($matches[1] as $name) {
                if (!in_array($name, $out, true)) {
                    $out[] = $name;
                }
            }
        }
        return $out;
    }

    /** @return iterable<int|string, mixed>|null */
    public static function dataSetsFor(\ReflectionClass $ref, \ReflectionMethod $m): ?iterable
    {
        // Collect provider sources from every supported style. Each source is a
        // `[?class, method]` pair: a null class means "the test class itself".
        // Vanilla PHPUnit treats #[DataProvider], #[DataProviderExternal] and the
        // legacy `@dataProvider` annotation (including its cross-class
        // `Class::method` form) uniformly — they all become DataProvider metadata
        // that flow through the same merge with the same duplicate-key check.
        //   - PHPUnit 10+ attribute: #[DataProvider('foo')]
        //   - PHPUnit 10+ attribute: #[DataProviderExternal(Other::class, 'foo')]
        //   - PHPUnit 9 / legacy:    @dataProvider foo               (same class)
        //   - PHPUnit 9 / legacy:    @dataProvider \Other::foo       (cross-class)
        // Many PHPUnit 10 codebases still use the PHPDoc style for back-compat,
        // and PHPUnit 9 has no attributes at all.
        //
        // @var list<array{class:?string,method:string}> $providerSources
        $providerSources = [];
        $addSource = static function (?string $class, string $method) use (&$providerSources): void {
            foreach ($providerSources as $existing) {
                if ($existing['class'] === $class && $existing['method'] === $method) {
                    return; // de-dupe identical declarations (mirrors prior name-set logic)
                }
            }
            $providerSources[] = ['class' => $class, 'method' => $method];
        };
        foreach ($m->getAttributes(DataProvider::class) as $attr) {
            $addSource(null, $attr->newInstance()->methodName());
        }
        // PHPUnit 10+ #[DataProviderExternal] — provider in a different class,
        // already require_once'd by the worker via required_files.
        foreach ($m->getAttributes(DataProviderExternal::class) as $attr) {
            $inst = $attr->newInstance();
            $addSource($inst->className(), $inst->methodName());
        }
        $doc = $m->getDocComment();
        if (is_string($doc) && preg_match_all('/@dataProvider\s+(\S+)/', $doc, $matches)) {
            foreach ($matches[1] as $name) {
                // Legacy cross-class form `@dataProvider \Some\Provider::rows`:
                // vanilla's annotation parser splits on '::' and reflects the
                // EXTERNAL class. The leading backslash (if any) is accepted by
                // ReflectionClass as-is, so we keep the token verbatim. Without
                // this split the planner reflected the literal 'Provider::rows'
                // on the TEST class, throwing ReflectionException and collapsing
                // the whole method to a single provider_error.
                if (str_contains($name, '::')) {
                    [$extClass, $extMeth] = explode('::', $name, 2);
                    $addSource($extClass, $extMeth);
                } else {
                    $addSource(null, $name);
                }
            }
        }

        // PHPUnit 10+ also supports inline data via #[TestWith([...])] and
        // #[TestWithJson('{"key": "val"}')]. Both are repeatable, each
        // instance contributes ONE row. Carbon makes heavy use of these.
        $testWithRows = [];
        foreach ($m->getAttributes(TestWith::class) as $attr) {
            $testWithRows[] = $attr->newInstance()->data();
        }
        foreach ($m->getAttributes(TestWithJson::class) as $attr) {
            $decoded = json_decode($attr->newInstance()->json(), true);
            if (is_array($decoded)) {
                $testWithRows[] = $decoded;
            }
        }

        // PHPUnit 9 / legacy PHPDoc style: `@testWith` followed by one
        // JSON array per line, optionally indented with the PHPDoc `*`
        // line marker:
        //
        //   /**
        //    * @testWith [10]
        //    *           [20]
        //    *           ["a string", 3]
        //    */
        //
        // Faker uses this heavily. PHPUnit's parser is line-based — it
        // walks each line, strips the leading `*`, and json_decodes any
        // line that starts with `[`. We do the same.
        if (is_string($doc) && ($twPos = strpos($doc, '@testWith')) !== false) {
            $after = substr($doc, $twPos + strlen('@testWith'));
            foreach (preg_split('/\R+/', $after) as $line) {
                $line = ltrim($line);
                // Strip PHPDoc line marker `*` (possibly preceded by space).
                if ($line !== '' && $line[0] === '*') {
                    $line = ltrim(substr($line, 1));
                }
                if ($line === '') continue;
                // Stop at next annotation or end-of-block marker.
                if ($line[0] === '@' || str_starts_with($line, '/')) break;
                if ($line[0] !== '[') continue;
                $row = json_decode($line, true);
                if (is_array($row)) $testWithRows[] = $row;
            }
        }

        if (empty($providerSources) && empty($testWithRows)) {
            return null;
        }

        // Merge every provider source into one ordered accumulator. String keys
        // are NAMED data sets: vanilla forbids the same named key being defined
        // twice across the providers of a single method and throws
        // InvalidDataProviderException. We mirror that loud per-method failure
        // (surfaced as a provider_error by plan()'s catch) instead of silently
        // last-winning. Integer keys are positional: PHP renumbers them on
        // append, so colliding int keys (e.g. two providers each starting at 0,
        // or `yield from [...]` segments) must append every row, never overwrite.
        $rows = [];
        foreach ($providerSources as $source) {
            // Resolve against the test class (null) or the named external class.
            $providerClassRef = $source['class'] === null
                ? $ref
                : new \ReflectionClass($source['class']);
            $providerRef = $providerClassRef->getMethod($source['method']);
            $providerRef->setAccessible(true);
            $result = $providerRef->isStatic()
                ? $providerRef->invoke(null)
                : $providerRef->invoke($providerClassRef->newInstanceWithoutConstructor());
            foreach ($result as $key => $row) {
                if (is_int($key)) {
                    $rows[] = $row;
                } else {
                    if (array_key_exists($key, $rows)) {
                        // Loud, per-method failure mirroring vanilla PHPUnit's
                        // DataProvider::dataProvidedByMethods. Thrown here so
                        // plan()'s existing catch turns it into one
                        // provider_error step for this method.
                        throw new \RuntimeException(sprintf(
                            'The key "%s" has already been defined by a previous data provider',
                            $key,
                        ));
                    }
                    $rows[$key] = $row;
                }
            }
        }
        // Append TestWith rows AFTER provider rows so PHPUnit-style dataset
        // indices match (provider rows numbered 0..N-1, TestWith rows N+).
        // TestWith rows are always positional (no named keys), so they append.
        foreach ($testWithRows as $row) {
            $rows[] = $row;
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
            // Defensive guard: a @depends target may reference a method
            // absent on the loaded class definition (FQCN conflict).
            if (!$ref->hasMethod($m)) return;
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
