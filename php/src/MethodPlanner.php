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
    private static function dataSetsFor(\ReflectionClass $ref, \ReflectionMethod $m): ?iterable
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
        if (empty($providerNames)) {
            return null;
        }
        $rows = [];
        foreach ($providerNames as $providerName) {
            $providerRef  = $ref->getMethod($providerName);
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
