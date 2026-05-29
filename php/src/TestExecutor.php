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
     * @param bool $isolated When true, the Rust runner has marked this class
     *        with a PHPUnit "run in separate process" annotation/attribute.
     *        Our worker is already a separate process per batch (the runner
     *        sets force_exit_after on these batches), so we must override
     *        PHPUnit's request to spawn a nested sub-process — otherwise its
     *        runtime would `proc_open()` a child PHP that hangs on FDs we
     *        leak across forks. We clear `runTestInSeparateProcess` on every
     *        instance before invocation.
     * @return list<array<string, mixed>>
     */
    public static function runClass(string $class, array $methods, ?array $rowFilter = null, bool $isolated = false): array
    {
        if (!is_subclass_of($class, TestCase::class)) {
            throw new \InvalidArgumentException("$class does not extend PHPUnit\\Framework\\TestCase");
        }

        $steps = MethodPlanner::plan($class, $methods, $rowFilter);
        $ref   = new \ReflectionClass($class);

        // Check class-level requirements BEFORE setUpBeforeClass. This is
        // critical: some test classes load PHP-version-specific entity files
        // inside setUpBeforeClass, which causes E_COMPILE_ERROR if we're on
        // the wrong PHP version (uncatchable — kills the worker process).
        $classSkipReason = self::checkRequires((string) $ref->getDocComment())
            ?? self::checkRequiresAttributes($ref->getAttributes());

        // Collect PHPUnit 10 lifecycle attribute methods once for the class.
        // Order of invocation, per PHPUnit docs, around each test:
        //   setUpBeforeClass + #[BeforeClass]   (once, before any test)
        //   setUp + #[Before] + #[PreCondition]  (each test)
        //   ...test...
        //   #[PostCondition] + #[After] + tearDown (each test)
        //   #[AfterClass] + tearDownAfterClass  (once, after all tests)
        $hooks = self::collectLifecycleHooks($ref);

        // setUpBeforeClass failure: emit one error outcome per method we
        // were going to run, then skip the body. Otherwise an exception
        // here aborts runClass entirely and we lose every per-test outcome
        // (vanilla emits N errors, one per test method).
        $setupBeforeError = null;
        if ($classSkipReason === null) {
            try {
                self::invokeOptionalStatic($ref, 'setUpBeforeClass');
                foreach ($hooks['before_class'] as $name) {
                    self::invokeStaticByName($ref, $name);
                }
            } catch (\Throwable $e) {
                $setupBeforeError = $e;
            }
        }

        $outcomes = [];
        $passedReturns = [];  // method -> return value, for @depends injection
        $givenCache = [];  // "$method\0$args_hash" → array (outcome shape)

        foreach ($steps as $step) {
            $method  = $step['method'];
            $dataset = $step['dataset'];
            $userArgs = $step['args'];
            $depends = $step['depends'];

            // Check cache for duplicate Given (identical method + args_hash).
            $cacheKey = (isset($step['args_hash']) && $step['args_hash'] !== null)
                ? $step['method'] . "\0" . $step['args_hash']
                : null;

            if (($step['is_duplicate'] ?? false) && $cacheKey !== null && isset($givenCache[$cacheKey])) {
                $cached = $givenCache[$cacheKey];
                $outcomes[] = array_merge($cached, [
                    'dataset' => $step['dataset'],
                    'message' => ($cached['message'] ?? null) !== null
                        ? $cached['message']
                        : '[memoized: identical Given]',
                ]);
                continue;
            }

            // Provider threw during planning — emit error and move on.
            if (isset($step['provider_error'])) {
                $outcomes[] = [
                    'class'       => $class,
                    'method'      => $method,
                    'dataset'     => null,
                    'status'      => 'error',
                    'message'     => 'data provider threw: ' . $step['provider_error'],
                    'trace'       => null,
                    'duration_ms' => 0.0,
                ];
                continue;
            }

            // setUpBeforeClass threw: every test in this batch errors with
            // the same message. Don't even reflect on the method — we may
            // not have loaded its dependencies. Emit and continue.
            if ($setupBeforeError !== null) {
                $outcomes[] = [
                    'class'       => $class,
                    'method'      => $method,
                    'dataset'     => $dataset,
                    'status'      => 'error',
                    'message'     => 'setUpBeforeClass: ' . $setupBeforeError->getMessage(),
                    'trace'       => $setupBeforeError->getTraceAsString(),
                    'duration_ms' => 0.0,
                ];
                continue;
            }

            // Method-level @requires takes precedence over class-level (PHPUnit
            // semantics: any failing @requires at either scope means skip).
            $methodRef = $ref->getMethod($method);
            $skipReason = $classSkipReason
                ?: self::checkRequires((string) $methodRef->getDocComment())
                ?: self::checkRequiresAttributes($methodRef->getAttributes());
            if ($skipReason !== null) {
                $outcomes[] = OutcomeBuilder::build(
                    $class, $method, $dataset, 0.0,
                    new \PHPUnit\Framework\IncompleteTestError($skipReason)
                );
                // Actually we want this as "skipped", not "incomplete". Build
                // the outcome array manually to ensure the right status.
                array_pop($outcomes);
                $outcomes[] = [
                    'class'       => $class,
                    'method'      => $method,
                    'dataset'     => $dataset,
                    'status'      => 'skipped',
                    'message'     => $skipReason,
                    'trace'       => null,
                    'duration_ms' => 0.0,
                ];
                continue;
            }

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
                if ($isolated) {
                    // Bind a closure into the TestCase scope so we can write
                    // the protected `runTestInSeparateProcess` flag directly.
                    // PHPUnit 10+ exposes `setRunTestInSeparateProcess()`, but
                    // the property exists on 9.x too and the closure path
                    // works uniformly across versions without relying on
                    // setter availability.
                    \Closure::bind(function () {
                        $this->runTestInSeparateProcess = false;
                        if (property_exists($this, 'runClassInSeparateProcess')) {
                            $this->runClassInSeparateProcess = false;
                        }
                    }, $test, TestCase::class)();
                }
                self::invokeOptional($test, 'setUp');
                foreach ($hooks['before'] as $name)         self::invokeInstanceByName($test, $name);
                foreach ($hooks['pre_condition'] as $name)  self::invokeInstanceByName($test, $name);
                $returnValue = $test->{$method}(...$args);
                foreach ($hooks['post_condition'] as $name) self::invokeInstanceByName($test, $name);
                foreach ($hooks['after'] as $name)          self::invokeInstanceByName($test, $name);
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
                    // Pass. Run post-test hooks + tearDown best-effort —
                    // any of these failing here would override the pass,
                    // which is consistent with vanilla PHPUnit.
                    try {
                        foreach ($hooks['after'] as $name) self::invokeInstanceByName($test, $name);
                        self::invokeOptional($test, 'tearDown');
                    } catch (\Throwable) { /* best-effort */ }
                } else {
                    $error = $e;
                }
            }

            $duration = (microtime(true) - $startedAt) * 1000.0;
            $outcomes[] = OutcomeBuilder::build($class, $method, $dataset, $duration, $error);

            // Store in cache (first occurrence only) for memoization.
            if ($cacheKey !== null && !isset($givenCache[$cacheKey])) {
                $givenCache[$cacheKey] = end($outcomes);
            }

            if ($error === null && $returnValue !== null) {
                $passedReturns[$method] = $returnValue;
            }
        }

        // tearDownAfterClass failure must NEVER lose the outcomes we've
        // accumulated for actual test methods. Earlier this was the dominant
        // cause of doctrine-orm's test-count gap (Doctrine\Tests\ORM\Functional\
        // ValueConversionType\*Test::tearDownAfterClass() calls executeStatement
        // on a null $sharedConn when no DB is configured, and dropped the
        // entire class's outcomes on the floor).
        if ($classSkipReason === null && $setupBeforeError === null) {
            try {
                foreach ($hooks['after_class'] as $name) {
                    self::invokeStaticByName($ref, $name);
                }
                self::invokeOptionalStatic($ref, 'tearDownAfterClass');
            } catch (\Throwable) {
                // Swallow; do not bias the run with a synthetic outcome.
                // Vanilla PHPUnit also doesn't add a separate outcome for
                // tearDownAfterClass failures.
            }
        }

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
     * Like invokeOptional but for an already-known method name from a
     * lifecycle attribute. The method is guaranteed to exist (we just
     * collected it via reflection) but may be protected/private; bind
     * the closure to the instance's scope to bypass visibility.
     */
    private static function invokeInstanceByName(TestCase $test, string $name): void
    {
        \Closure::bind(function () use ($name) {
            // @phpstan-ignore-next-line
            $this->{$name}();
        }, $test, $test)();
    }

    /**
     * Static-method counterpart for #[BeforeClass] / #[AfterClass] hooks
     * we collected. Method existence already verified at collection time.
     */
    private static function invokeStaticByName(\ReflectionClass $ref, string $name): void
    {
        $m = $ref->getMethod($name);
        $m->setAccessible(true);
        $m->invoke(null);
    }

    /**
     * Walk the class's methods and, for each, collect the lifecycle
     * attributes that decorate it. PHPUnit recognises six:
     *
     *   #[BeforeClass] / #[AfterClass]    static, around the class
     *   #[Before] / #[After]              instance, around each test
     *   #[PreCondition] / #[PostCondition] instance, *inside* the
     *                                       setUp/tearDown sandwich
     *
     * Each attribute is repeatable and a single method may carry several.
     * The class's parent chain is walked so inherited hooks fire too.
     * Order within one attribute kind is "as declared, parent-first".
     *
     * @return array{
     *   before_class: list<string>, after_class: list<string>,
     *   before: list<string>, after: list<string>,
     *   pre_condition: list<string>, post_condition: list<string>,
     * }
     */
    private static function collectLifecycleHooks(\ReflectionClass $ref): array
    {
        $hooks = [
            'before_class'   => [],
            'after_class'    => [],
            'before'         => [],
            'after'          => [],
            'pre_condition'  => [],
            'post_condition' => [],
        ];
        $kinds = [
            'PHPUnit\\Framework\\Attributes\\BeforeClass'    => 'before_class',
            'PHPUnit\\Framework\\Attributes\\AfterClass'     => 'after_class',
            'PHPUnit\\Framework\\Attributes\\Before'         => 'before',
            'PHPUnit\\Framework\\Attributes\\After'          => 'after',
            'PHPUnit\\Framework\\Attributes\\PreCondition'   => 'pre_condition',
            'PHPUnit\\Framework\\Attributes\\PostCondition'  => 'post_condition',
        ];

        // Walk parent-first so inherited hooks run before subclass ones,
        // matching PHPUnit's behaviour. A child can dedup by overriding
        // a parent's hook with the same method name.
        $chain = [];
        for ($c = $ref; $c !== false; $c = $c->getParentClass()) {
            array_unshift($chain, $c);
        }
        $seen = [];  // (kind, method_name) -> true
        foreach ($chain as $c) {
            foreach ($c->getMethods() as $m) {
                foreach ($m->getAttributes() as $attr) {
                    $kind = $kinds[$attr->getName()] ?? null;
                    if ($kind === null) continue;
                    $name = $m->getName();
                    $key = "$kind\0$name";
                    if (isset($seen[$key])) continue;
                    $seen[$key] = true;
                    $hooks[$kind][] = $name;
                }
            }
        }
        return $hooks;
    }

    /**
     * Evaluate `@requires` annotations in a PHPDoc block. Returns null if all
     * requirements are satisfied (or there are none), or a human-readable
     * "skip" message if any requirement fails.
     *
     * Supported requirement kinds (the common subset; PHPUnit has more):
     *   - `@requires PHP <op><version>`     e.g. `@requires PHP >= 8.0`
     *   - `@requires PHP <version>`          (no operator → ">=")
     *   - `@requires PHP < 7.4` etc.
     *   - `@requires extension <name>`       skip if extension not loaded
     *   - `@requires function <name>`        skip if function not declared
     *   - `@requires OS <regex>`             skip if `php_uname('s')` doesn't match
     */
    private static function checkRequires(string $doc): ?string
    {
        if ($doc === '') return null;
        if (!preg_match_all('/@requires\s+(\S+)\s+(.+)/', $doc, $matches, PREG_SET_ORDER)) {
            return null;
        }
        foreach ($matches as $m) {
            $kind = $m[1];
            $spec = trim($m[2]);
            switch ($kind) {
                case 'PHP':
                    // Allow leading operator, default to `>=`.
                    if (!preg_match('/^(<=|>=|<>|!=|==|=|<|>)?\s*(.+)$/', $spec, $vm)) {
                        continue 2;
                    }
                    $op = $vm[1] !== '' ? $vm[1] : '>=';
                    if ($op === '=') $op = '==';
                    $required = $vm[2];
                    if (!version_compare(PHP_VERSION, $required, $op)) {
                        return "PHP {$op} {$required} (have " . PHP_VERSION . ')';
                    }
                    break;
                case 'extension':
                    $ext = strtok($spec, ' ');
                    if (!extension_loaded($ext)) {
                        return "extension {$ext} not loaded";
                    }
                    break;
                case 'function':
                    if (!function_exists($spec)) {
                        return "function {$spec} not defined";
                    }
                    break;
                case 'OS':
                    if (!preg_match('/' . str_replace('/', '\\/', $spec) . '/i', PHP_OS)) {
                        return "OS does not match {$spec} (have " . PHP_OS . ')';
                    }
                    break;
                default:
                    // Ignore unknown @requires kinds (PHPUnit, OSFAMILY, etc.)
            }
        }
        return null;
    }

    /**
     * Evaluate PHPUnit 10 PHP-8-attribute-based requirements on a class or
     * method. Returns null if all requirements are satisfied, or a skip
     * message if any requirement fails.
     *
     * @param \ReflectionAttribute[] $attrs
     */
    private static function checkRequiresAttributes(array $attrs): ?string
    {
        foreach ($attrs as $attr) {
            $name = $attr->getName();
            switch ($name) {
                case 'PHPUnit\\Framework\\Attributes\\RequiresPhp':
                    $inst = $attr->newInstance();
                    $req = $inst->versionRequirement();
                    // Semver caret: ^X.Y  →  >=X.Y.0 <(X+1).0.0
                    if (preg_match('/^\^(\d+)\.(\d+)(?:\.(\d+))?$/', $req, $cm)) {
                        $major = (int) $cm[1];
                        $minor = (int) $cm[2];
                        $patch = isset($cm[3]) ? (int) $cm[3] : 0;
                        $low  = "{$major}.{$minor}.{$patch}";
                        $high = ($major + 1) . '.0.0';
                        if (!version_compare(PHP_VERSION, $low, '>=')
                            || !version_compare(PHP_VERSION, $high, '<')) {
                            return "PHP ^{$major}.{$minor} required (have " . PHP_VERSION . ')';
                        }
                        break;
                    }
                    // Semver tilde: ~X.Y  →  >=X.Y.0 <X.(Y+1).0
                    if (preg_match('/^~(\d+)\.(\d+)(?:\.(\d+))?$/', $req, $tm)) {
                        $major = (int) $tm[1];
                        $minor = (int) $tm[2];
                        $patch = isset($tm[3]) ? (int) $tm[3] : 0;
                        $low  = "{$major}.{$minor}.{$patch}";
                        $high = "{$major}." . ($minor + 1) . '.0';
                        if (!version_compare(PHP_VERSION, $low, '>=')
                            || !version_compare(PHP_VERSION, $high, '<')) {
                            return "PHP ~{$major}.{$minor} required (have " . PHP_VERSION . ')';
                        }
                        break;
                    }
                    if (!preg_match('/^(<=|>=|<>|!=|==|=|<|>)?\s*(.+)$/', $req, $vm)) break;
                    $op = ($vm[1] !== '' ? $vm[1] : '>=');
                    if ($op === '=') $op = '==';
                    if (!version_compare(PHP_VERSION, $vm[2], $op)) {
                        return "PHP {$op} {$vm[2]} required (have " . PHP_VERSION . ')';
                    }
                    break;
                case 'PHPUnit\\Framework\\Attributes\\RequiresPhpExtension':
                    $inst = $attr->newInstance();
                    $ext = $inst->extension();
                    if (!extension_loaded($ext)) {
                        return "extension {$ext} not loaded";
                    }
                    break;
                case 'PHPUnit\\Framework\\Attributes\\RequiresFunction':
                    $inst = $attr->newInstance();
                    $fn = $inst->functionName();
                    if (!function_exists($fn)) {
                        return "function {$fn} not defined";
                    }
                    break;
                case 'PHPUnit\\Framework\\Attributes\\RequiresMethod':
                    $inst = $attr->newInstance();
                    if (!method_exists($inst->className(), $inst->methodName())) {
                        return "method {$inst->className()}::{$inst->methodName()} not defined";
                    }
                    break;
                case 'PHPUnit\\Framework\\Attributes\\RequiresOperatingSystem':
                    $inst = $attr->newInstance();
                    $pattern = $inst->regularExpression();
                    if (!preg_match('/' . str_replace('/', '\\/', $pattern) . '/i', PHP_OS)) {
                        return "OS does not match {$pattern} (have " . PHP_OS . ')';
                    }
                    break;
                case 'PHPUnit\\Framework\\Attributes\\RequiresOperatingSystemFamily':
                    $inst = $attr->newInstance();
                    $family = $inst->operatingSystemFamily();
                    if (stripos(PHP_OS_FAMILY, $family) === false) {
                        return "OS family {$family} required (have " . PHP_OS_FAMILY . ')';
                    }
                    break;
                case 'PHPUnit\\Framework\\Attributes\\RequiresSetting':
                    $inst = $attr->newInstance();
                    $setting = $inst->setting();
                    $expected = $inst->value();
                    $actual = ini_get($setting);
                    if ((string) $actual !== (string) $expected) {
                        return "ini {$setting}={$expected} required (have " . var_export($actual, true) . ')';
                    }
                    break;
                case 'PHPUnit\\Framework\\Attributes\\RequiresPhpunit':
                    $inst = $attr->newInstance();
                    $req = $inst->versionRequirement();
                    if (!class_exists('PHPUnit\\Runner\\Version')) break;
                    $current = \PHPUnit\Runner\Version::id();
                    if (!preg_match('/^(<=|>=|<>|!=|==|=|<|>)?\s*(.+)$/', $req, $vm)) break;
                    $op = ($vm[1] !== '' ? $vm[1] : '>=');
                    if ($op === '=') $op = '==';
                    if (!version_compare($current, $vm[2], $op)) {
                        return "PHPUnit {$op} {$vm[2]} required (have {$current})";
                    }
                    break;
            }
        }
        return null;
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
