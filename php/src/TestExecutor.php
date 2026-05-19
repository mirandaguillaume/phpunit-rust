<?php

declare(strict_types=1);

namespace PhpunitRust;

use PHPUnit\Framework\TestCase;

/**
 * Runs every step of a planned class invocation. Handles setUp/tearDown,
 * setUpBeforeClass/tearDownAfterClass, expectException, and dependency
 * return-value injection. No knowledge of PHPUnit's TestRunner, Facade,
 * or Configuration — we only call into the user-facing TestCase API.
 */
final class TestExecutor
{
    /**
     * @param class-string $class
     * @param list<string> $methods Empty = all `test*` methods on the class.
     * @return list<array<string, mixed>>
     */
    public static function runClass(string $class, array $methods): array
    {
        if (!is_subclass_of($class, TestCase::class)) {
            throw new \InvalidArgumentException("$class does not extend PHPUnit\\Framework\\TestCase");
        }

        $steps = MethodPlanner::plan($class, $methods);
        $ref   = new \ReflectionClass($class);

        // setUpBeforeClass (PHPUnit's per-class hook, protected static).
        self::invokeOptionalStatic($ref, 'setUpBeforeClass');

        $outcomes = [];
        $passedReturns = [];  // method -> return value, for @depends injection

        foreach ($steps as $step) {
            $method  = $step['method'];
            $dataset = $step['dataset'];
            $userArgs = $step['args'];
            $depends = $step['depends'];

            // Build args = depends-return-values ++ data-provider-args.
            $args = [];
            $missingDep = null;
            foreach ($depends as $dep) {
                if (!array_key_exists($dep, $passedReturns)) {
                    $missingDep = $dep;
                    break;
                }
                $args[] = $passedReturns[$dep];
            }
            if ($missingDep !== null) {
                $outcomes[] = OutcomeBuilder::build(
                    $class, $method, $dataset, 0.0,
                    new \PHPUnit\Framework\SkippedWithMessageException(
                        "missing dependency: {$missingDep}"
                    )
                );
                continue;
            }
            $args = array_merge($args, $userArgs);

            $startedAt = microtime(true);
            $error = null;
            $returnValue = null;

            try {
                $test = new $class($method);
                self::invokeOptional($test, 'setUp');
                $returnValue = $test->{$method}(...$args);
                self::invokeOptional($test, 'tearDown');

                // expectException* check: if the test declared any expectation
                // (class, message, or code) and no exception was thrown,
                // that's a failure. PHPUnit's runner verifies this via the
                // runBare flow we're bypassing.
                $expectedClass = self::readExpectedException($test);
                $expectedMessage = self::readPrivateProp($test, 'expectedExceptionMessage');
                $expectedCode = self::readPrivateProp($test, 'expectedExceptionCode');
                if (
                    $expectedClass !== null
                    || (is_string($expectedMessage) && $expectedMessage !== '')
                    || ($expectedCode !== null && $expectedCode !== '')
                ) {
                    $desc = $expectedClass ?? 'exception';
                    if (is_string($expectedMessage) && $expectedMessage !== '') {
                        $desc .= " with message containing \"{$expectedMessage}\"";
                    }
                    $error = new \PHPUnit\Framework\ExpectationFailedException(
                        "Expected {$desc} was not thrown"
                    );
                }
            } catch (\PHPUnit\Framework\SkippedWithMessageException $e) {
                $error = $e;
            } catch (\PHPUnit\Framework\IncompleteTestError $e) {
                $error = $e;
            } catch (\Throwable $e) {
                // Was this exception expected via expectException()?
                if (isset($test) && self::wasExceptionExpected($test, $e)) {
                    // Pass. Run tearDown if setUp succeeded — best-effort.
                    self::invokeOptional($test, 'tearDown');
                } else {
                    $error = $e;
                }
            }

            $duration = (microtime(true) - $startedAt) * 1000.0;
            $outcomes[] = OutcomeBuilder::build($class, $method, $dataset, $duration, $error);

            if ($error === null && $returnValue !== null) {
                $passedReturns[$method] = $returnValue;
            }
        }

        self::invokeOptionalStatic($ref, 'tearDownAfterClass');

        return $outcomes;
    }

    private static function invokeOptional(TestCase $test, string $name): void
    {
        // Bind a closure to the test instance's scope so we can call protected
        // setUp / tearDown without ReflectionMethod::setAccessible (deprecated
        // in PHP 8.1+ when applied to non-private members).
        \Closure::bind(function () use ($name) {
            // @phpstan-ignore-next-line
            $this->{$name}();
        }, $test, $test)();
    }

    private static function invokeOptionalStatic(\ReflectionClass $ref, string $name): void
    {
        if (!$ref->hasMethod($name)) return;
        $m = $ref->getMethod($name);
        if (!$m->isStatic()) return;
        $m->setAccessible(true);
        $m->invoke(null);
    }

    /**
     * Read one of TestCase's private "expected*" properties via reflection.
     * Returns null if the property doesn't exist (PHPUnit version drift) or
     * is unset on this instance.
     */
    private static function readPrivateProp(TestCase $test, string $name)
    {
        $ref = new \ReflectionClass(TestCase::class);
        if (!$ref->hasProperty($name)) return null;
        $prop = $ref->getProperty($name);
        $prop->setAccessible(true);
        return $prop->getValue($test);
    }

    /**
     * Returns the FQCN of the exception class declared via expectException(),
     * or null if none was declared.
     */
    private static function readExpectedException(TestCase $test): ?string
    {
        $value = self::readPrivateProp($test, 'expectedException');
        return is_string($value) ? $value : null;
    }

    /**
     * Does the thrown exception match the test's `expectException*` setup?
     *
     * PHPUnit honors three separately-settable expectations:
     *   - expectException(class)              → throw must be instance of class
     *   - expectExceptionMessage(substring)   → throw->getMessage must contain substring
     *   - expectExceptionCode(code)           → throw->getCode must equal code
     *
     * If ANY of the three is set, an exception is expected. The throw matches
     * iff every set expectation is satisfied. A test that only sets
     * `expectExceptionMessage('foo')` (no class) implicitly expects any
     * Throwable whose message contains 'foo' — that's PHPUnit's semantics.
     */
    private static function wasExceptionExpected(TestCase $test, \Throwable $thrown): bool
    {
        $expectedClass   = self::readExpectedException($test);
        $expectedMessage = self::readPrivateProp($test, 'expectedExceptionMessage');
        $expectedCode    = self::readPrivateProp($test, 'expectedExceptionCode');

        $anySet = $expectedClass !== null
            || (is_string($expectedMessage) && $expectedMessage !== '')
            || ($expectedCode !== null && $expectedCode !== '');
        if (!$anySet) {
            return false;
        }

        if ($expectedClass !== null && !($thrown instanceof $expectedClass)) {
            return false;
        }
        if (is_string($expectedMessage) && $expectedMessage !== ''
            && strpos((string) $thrown->getMessage(), $expectedMessage) === false) {
            return false;
        }
        if ($expectedCode !== null && $expectedCode !== ''
            && (string) $thrown->getCode() !== (string) $expectedCode) {
            return false;
        }
        return true;
    }
}
