<?php

declare(strict_types=1);

namespace PhpunitRust;

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
 * @phpstan-type Step array{method:string,dataset:?string,args:list<mixed>,depends:list<string>}
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
                $rowHash     = md5(json_encode(array_values($rowData)));
                $isDuplicate = isset($seenHashes[$rowHash]);
                $seenHashes[$rowHash] = true;
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
        // Collect provider method names from both styles:
        //   - PHPUnit 10+ attribute: #[DataProvider('foo')]
        //   - PHPUnit 9 / legacy:    @dataProvider foo  (in PHPDoc)
        // Many PHPUnit 10 codebases still use the PHPDoc style for back-compat,
        // and PHPUnit 9 has no attributes at all.
        $providerNames = [];
        foreach ($m->getAttributes(DataProvider::class) as $attr) {
            $providerNames[] = $attr->newInstance()->methodName();
        }
        $doc = $m->getDocComment();
        if (is_string($doc) && preg_match_all('/@dataProvider\s+(\S+)/', $doc, $matches)) {
            foreach ($matches[1] as $name) {
                if (!in_array($name, $providerNames, true)) {
                    $providerNames[] = $name;
                }
            }
        }

        // PHPUnit 10+ #[DataProviderExternal] — provider in a different class,
        // already require_once'd by the worker via required_files.
        $externalRows = [];
        foreach ($m->getAttributes(DataProviderExternal::class) as $attr) {
            $inst     = $attr->newInstance();
            $extClass = $inst->className();
            $extMeth  = $inst->methodName();
            foreach ($extClass::$extMeth() as $key => $row) {
                if (is_int($key)) { $externalRows[] = $row; }
                else              { $externalRows[$key] = $row; }
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

        if (empty($providerNames) && empty($testWithRows) && empty($externalRows)) {
            return null;
        }

        $rows = [];
        foreach ($providerNames as $providerName) {
            $providerRef = $ref->getMethod($providerName);
            $providerRef->setAccessible(true);
            $result = $providerRef->isStatic()
                ? $providerRef->invoke(null)
                : $providerRef->invoke($ref->newInstanceWithoutConstructor());
            foreach ($result as $key => $row) {
                // Integer keys (the default from yield-without-key and from
                // bare array literals like `yield from [...]`) collide across
                // generator segments — both start at 0. Append for ints to
                // preserve every row; keep string keys for named data sets.
                if (is_int($key)) {
                    $rows[] = $row;
                } else {
                    $rows[$key] = $row;
                }
            }
        }
        // Append TestWith rows AFTER provider rows so PHPUnit-style dataset
        // indices match (provider rows numbered 0..N-1, TestWith rows N+).
        foreach ($testWithRows as $row) {
            $rows[] = $row;
        }
        foreach ($externalRows as $key => $row) {
            if (is_int($key)) { $rows[] = $row; }
            else              { $rows[$key] = $row; }
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
