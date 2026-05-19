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
    private static function dependsOf(\ReflectionMethod $m): array
    {
        $out = [];
        foreach ($m->getAttributes(Depends::class) as $attr) {
            $instance = $attr->newInstance();
            // Depends::methodName() is the public accessor in PHPUnit 10+.
            $out[] = $instance->methodName();
        }
        return $out;
    }

    /** @return iterable<int|string, mixed>|null */
    private static function dataSetsFor(\ReflectionClass $ref, \ReflectionMethod $m): ?iterable
    {
        $providers = $m->getAttributes(DataProvider::class);
        if (empty($providers)) {
            return null;
        }
        $rows = [];
        foreach ($providers as $attr) {
            $providerName = $attr->newInstance()->methodName();
            $providerRef  = $ref->getMethod($providerName);
            $providerRef->setAccessible(true);
            $result = $providerRef->isStatic()
                ? $providerRef->invoke(null)
                : $providerRef->invoke($ref->newInstanceWithoutConstructor());
            foreach ($result as $key => $row) {
                $rows[$key] = $row;
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
