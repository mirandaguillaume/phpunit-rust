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
        $classSkipReason = self::classSkipReason($class);

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
        $reqSkipped = [];     // method -> true once its requirement-skip is emitted

        foreach ($steps as $step) {
            $method  = $step['method'];
            $dataset = $step['dataset'];
            $userArgs = $step['args'];
            $depends = $step['depends'];

            // Parity (P4): we DELIBERATELY do not memoize/replay byte-identical
            // data-provider rows. Vanilla PHPUnit executes every row, even two
            // identical ones — replaying row 1's cached outcome for row 2 masks
            // state-dependent divergence (static counters, filesystem artifacts,
            // a row whose body mutates process-global state). MethodPlanner still
            // computes `is_duplicate` (that flag is owned elsewhere), but we no
            // longer CONSUME it here: every row runs the body exactly once.

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

            // Empty data provider: PHPUnit reports the method as ONE skipped
            // test ("skipped by data provider"), never zero — emit it so the
            // count matches vanilla instead of the method silently vanishing.
            if (!empty($step['empty_provider'])) {
                $outcomes[] = [
                    'class'       => $class,
                    'method'      => $method,
                    'dataset'     => null,
                    'status'      => 'skipped',
                    'message'     => 'Skipped: data provider provided no data',
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
                // How a requirement-gated data-provider method is counted DIFFERS
                // by PHPUnit major, so match the project's PHPUnit:
                //   >= 10: ONE skipped test per method; the provider is not
                //          expanded (monolog @ PHPUnit 11: testConstruct = 1).
                //   <= 9 : the provider IS expanded and EACH row is skipped
                //          (faker @ PHPUnit 9.6: testLastNameFemale = 6 skips).
                // Collapsing 9.x to 1 (or expanding >=10 to N) diverges from
                // vanilla's count. The skip reason is identical for every row.
                if (self::phpunitMajor() >= 10) {
                    if (isset($reqSkipped[$method])) {
                        continue;  // already emitted this method's single skip
                    }
                    $reqSkipped[$method] = true;
                    $dataset = null;  // collapsed: a single dataset-less skip
                }
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
            // Per-test DB isolation handle (P2). Resolved inside the try below
            // so a connection failure is reported as this test's error rather
            // than aborting runClass. Null on non-DB runs (zero overhead).
            $txPdo = null;
            // P6: per-test $GLOBALS snapshot when the test opts into
            // backupGlobals. Null when not requested (zero overhead otherwise).
            $globalsBackup = null;
            // P3: each test gets its own output buffer so expectOutputString()/
            // expectOutputRegex() can be asserted against exactly this test's
            // echo, and so stray output never bleeds into the batch stream.
            $obStarted = false;

            // Construct the TestCase before the body try. A constructor throw
            // is exceptional (TestCase's own constructor only records the name),
            // but if it happens we must record THIS test as an error and move
            // on — one test failing must never abort the rest of the batch.
            try {
                $test = new $class($method);
            } catch (\Throwable $ctorEx) {
                $outcomes[] = OutcomeBuilder::build(
                    $class, $method, $dataset,
                    (microtime(true) - $startedAt) * 1000.0,
                    $ctorEx
                );
                continue;
            }
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

            // P6: snapshot $GLOBALS BEFORE setUp when the class/method opts in.
            // Vanilla snapshots in runBare before the before-hooks fire so any
            // global mutation by setUp/the body/after-hooks is rolled back.
            $backupGlobals = self::backupGlobalsRequested($ref, $method);
            if ($backupGlobals) {
                $globalsBackup = self::snapshotGlobals(
                    self::backupGlobalsExcludeList($ref, $method)
                );
            }

            // BODY try: setUp + before/precondition hooks + the test body only.
            // Crucially this try does NOT contain the after-hooks/tearDown —
            // vanilla runs those in a SEPARATE try so they fire even when the
            // body throws, while preserving the ORIGINAL exception (P1).
            try {
                // P3: open this test's own output buffer just before the body.
                ob_start();
                $obStarted = true;

                // P2: resolve the per-slot handle and begin a transaction
                // before setUp, so every write made by setUp / the test /
                // tearDown is rolled back afterwards. We bypass PHPUnit's
                // runBare, so framework traits (RefreshDatabase,
                // DatabaseTransactions) NEVER fire — this plain-PDO
                // transaction is the only correct per-test isolation boundary.
                $txPdo = self::dbHandle();
                if ($txPdo !== null && !$txPdo->inTransaction()) {
                    $txPdo->beginTransaction();
                }
                self::invokeOptional($test, 'setUp');
                foreach ($hooks['before'] as $name)         self::invokeInstanceByName($test, $name);
                foreach ($hooks['pre_condition'] as $name)  self::invokeInstanceByName($test, $name);
                $returnValue = $test->{$method}(...$args);
                foreach ($hooks['post_condition'] as $name) self::invokeInstanceByName($test, $name);

                // expectException* check: if the test declared any expectation
                // (class, message, code, OR message-regex) and no exception was
                // thrown, that's a failure. PHPUnit's runner verifies this via
                // the runBare flow we're bypassing (expectedExceptionWasNotRaised).
                $expectedClass     = self::readExpectedException($test);
                $expectedMessage   = self::readPrivateProp($test, 'expectedExceptionMessage');
                $expectedCode      = self::readPrivateProp($test, 'expectedExceptionCode');
                $expectedMsgRegExp = self::readPrivateProp($test, 'expectedExceptionMessageRegExp');
                if (
                    $expectedClass !== null
                    || (is_string($expectedMessage) && $expectedMessage !== '')
                    || ($expectedCode !== null && $expectedCode !== '')
                    || (is_string($expectedMsgRegExp) && $expectedMsgRegExp !== '')
                ) {
                    $desc = $expectedClass ?? 'exception';
                    if (is_string($expectedMessage) && $expectedMessage !== '') {
                        $desc .= " with message containing \"{$expectedMessage}\"";
                    }
                    if (is_string($expectedMsgRegExp) && $expectedMsgRegExp !== '') {
                        $desc .= " with message matching \"{$expectedMsgRegExp}\"";
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
                // Was this exception expected via expectException* setup?
                if (self::wasExceptionExpected($test, $e)) {
                    // Pass: the throw matched every set expectation. $error stays
                    // null; after-hooks + tearDown still run below (separate try).
                } else {
                    $error = $e;
                }
            }

            // P3: assert output expectations exactly like vanilla's
            // performAssertionsOnOutput — but ONLY when the body did not already
            // error, mirroring runBare's `if (!isset($e) && hasExpectationOnOutput)`.
            // Capture+close this test's buffer regardless so it never leaks.
            if ($obStarted) {
                $output = ob_get_clean();
                $obStarted = false;
                if ($error === null) {
                    $error = self::assertOutputExpectations($test, (string) $output);
                }
                // No output expectation, or already errored: vanilla discards
                // the buffered output (it is not re-echoed into the parent).
            }

            // TEARDOWN try (P1): #[After] hooks + tearDown ALWAYS run, even when
            // the body threw. Vanilla TestCase::runBare runs these in a separate
            // try: "An exception raised in tearDown() will be caught and passed
            // on when no exception was raised before." So a teardown throw only
            // becomes the outcome's error when the test itself succeeded; it
            // must never mask the test's own failure.
            try {
                foreach ($hooks['after'] as $name) self::invokeInstanceByName($test, $name);
                self::invokeOptional($test, 'tearDown');
            } catch (\Throwable $teardownEx) {
                if ($error === null) {
                    $error = $teardownEx;
                }
            }

            // P2: roll back unconditionally. Microsecond cost. Reset, not
            // recreate. Guarded so non-DB runs pay zero (txPdo is null).
            if ($txPdo !== null) {
                if ($txPdo->inTransaction()) {
                    try {
                        $txPdo->rollBack();
                    } catch (\Throwable) {
                        // best-effort: a connection death already surfaces as
                        // the test error; don't mask it with a rollback throw.
                    }
                } else {
                    // P5: the transaction is gone but we opened one — the test
                    // committed (explicit commit() or a DDL implicit commit).
                    // We CANNOT roll those writes back; they leak into the slot
                    // clone for every later test in this worker. Full re-clone is
                    // out of scope (resource-provisioning design), so the
                    // documented mitigation is a loud forensic breadcrumb, in the
                    // STDERR style worker_fork.php uses for its own warnings.
                    $slot = getenv('PHPUNIT_RUST_SLOT');
                    $slot = ($slot === false || $slot === '') ? '?' : $slot;
                    fwrite(STDERR, sprintf(
                        "TestExecutor: DB isolation LEAK — %s::%s committed inside its "
                        . "transaction (slot=%s); writes are NOT rolled back and will "
                        . "leak into later tests in this worker\n",
                        $class,
                        $method,
                        $slot
                    ));
                }
            }

            // P6: restore $GLOBALS from the pre-test snapshot, undoing any
            // mutation the test made. Done after teardown so teardown can still
            // read the (mutated) globals, matching vanilla's restore-after-tear.
            if ($globalsBackup !== null) {
                self::restoreGlobals(
                    $globalsBackup,
                    self::backupGlobalsExcludeList($ref, $method)
                );
            }

            $duration = (microtime(true) - $startedAt) * 1000.0;
            $outcomes[] = OutcomeBuilder::build($class, $method, $dataset, $duration, $error);

            // A passing test satisfies @depends regardless of its return value:
            // PHPUnit injects the return (null for a void dependency) into the
            // dependent. The consumer side uses array_key_exists(), so recording
            // even a null here is what makes a void-returning dependency count as
            // satisfied. The old `!== null` guard wrongly skipped ~40 doctrine-orm
            // tests as "missing dependency".
            if ($error === null) {
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
     * Class-level skip reason from `@requires` (docblock) and `#[Requires*]`
     * (attributes), or null if all the class's requirements are satisfied.
     * Shared by runClass and by enumerate_providers.php — the enumerator uses
     * it to avoid stride-splitting a gated class's heavy data-provider method
     * (each split emits its own skip; on PHPUnit >=10 vanilla emits exactly one,
     * so a split would over-count by chunks-1).
     */
    public static function classSkipReason(string $class): ?string
    {
        if (!class_exists($class)) {
            return null;
        }
        $ref = new \ReflectionClass($class);
        return self::checkRequires((string) $ref->getDocComment())
            ?? self::checkRequiresAttributes($ref->getAttributes());
    }

    /**
     * Method-level skip reason: the class-level requirements PLUS the method's
     * own `@requires` / `#[Requires*]`, or null if nothing gates it. Mirrors the
     * precedence runClass applies per step (class gate first, then the method's
     * docblock, then its attributes). Shared with enumerate_providers.php, which
     * uses it to refuse stride-splitting a heavy provider whose consuming test
     * method is itself gated — on PHPUnit >=10 vanilla emits a single collapsed
     * skip, so a split would over-count by chunks-1. An absent method (or class)
     * yields null rather than throwing.
     *
     * @param class-string $class
     */
    public static function methodSkipReason(string $class, string $method): ?string
    {
        if (!class_exists($class)) {
            return null;
        }
        $classSkip = self::classSkipReason($class);
        if ($classSkip !== null) {
            return $classSkip;
        }
        $ref = new \ReflectionClass($class);
        if (!$ref->hasMethod($method)) {
            return null;
        }
        $methodRef = $ref->getMethod($method);
        return self::checkRequires((string) $methodRef->getDocComment())
            ?? self::checkRequiresAttributes($methodRef->getAttributes());
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
     * Major version of the PROJECT's loaded PHPUnit (e.g. 9, 10, 11), or 0 if
     * it cannot be determined. Used to match version-specific behavior such as
     * how a requirement-gated data-provider method is counted (9.x expands and
     * skips each row; >=10 reports the method as a single skipped test).
     */
    private static function phpunitMajor(): int
    {
        static $major = null;
        if ($major === null) {
            $major = class_exists('\\PHPUnit\\Runner\\Version')
                ? (int) \PHPUnit\Runner\Version::id()
                : 0;
        }
        return $major;
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
     * The runner-managed PDO connection used for per-test transaction isolation.
     * Tests and application code MUST obtain their DB connection from here (or be
     * configured to route through it) for the begin/rollback reset to isolate their
     * writes. Code that opens its OWN connection is NOT isolated by the runner and
     * must be marked stateful (#[UsesDatabase] does not make ad-hoc connections
     * isolated). Returns null when PHPUNIT_RUST_DB_DSN is unset (inert).
     */
    public static function connection(): ?\PDO
    {
        return self::dbHandle();
    }

    /**
     * Resolve a single, per-process PDO handle for per-test transaction
     * isolation (P2), or null when no per-slot DSN was injected.
     *
     * The DSN comes from PHPUNIT_RUST_DB_DSN (the per-slot env var set by the
     * fork-worker master). We memoize the handle per worker process: opening a
     * new PDO per test would cost latency AND, for sqlite::memory:, create a
     * fresh empty DB each time — breaking cross-test visibility. Memoization
     * keys on $dsnAtConnect: the fast path returns the cached handle when
     * $pdo !== null && $dsnAtConnect === $dsn, otherwise we (re)connect. In
     * production a worker's DSN never changes, so the first call connects and
     * every later call hits the fast path; a non-DB run pays one getenv only.
     *
     * A DSN that is set but unusable is NOT swallowed: the PDO constructor's
     * exception propagates so the per-test try/catch records that test as an
     * Error with the real connection message, rather than silently running it
     * unisolated and reporting green.
     */
    private static function dbHandle(): ?\PDO
    {
        static $pdo = null;
        static $dsnAtConnect = null; // DSN we connected with, for memoization

        // Inert path: no DSN injected. Drop any stale handle so that if the
        // DSN is later set again — even to the same string, as the test
        // harness does in setUp/tearDown — we open a FRESH connection instead
        // of returning a handle to an unlinked-and-recreated sqlite inode.
        // Production never re-injects a DSN, so this only removes a test flake.
        $dsn = getenv('PHPUNIT_RUST_DB_DSN');
        if ($dsn === false || $dsn === '') {
            $pdo = null;
            $dsnAtConnect = null;
            return null;
        }

        // Fast path: reuse the existing connection if the DSN hasn't changed.
        // In production a worker's DSN never changes, so this always hits
        // after the first call. One getenv per test is negligible.
        if ($pdo !== null && $dsnAtConnect === $dsn) {
            return $pdo;
        }

        // A DSN IS set: connect. Do NOT swallow a connection failure — let it
        // propagate so the per-test try/catch records a loud, diagnosable
        // Error rather than silently running the test without isolation.
        $pdo = new \PDO($dsn, null, null, [\PDO::ATTR_ERRMODE => \PDO::ERRMODE_EXCEPTION]);
        $dsnAtConnect = $dsn;

        return $pdo;
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
     * PHPUnit honors a fourth expectation too:
     *   - expectExceptionMessageMatches(regex) → throw->getMessage must match regex
     *
     * If ANY of these is set, an exception is expected. The throw matches iff
     * every set expectation is satisfied. A test that only sets
     * `expectExceptionMessage('foo')` (no class) implicitly expects any
     * Throwable whose message contains 'foo' — that's PHPUnit's semantics.
     */
    private static function wasExceptionExpected(TestCase $test, \Throwable $thrown): bool
    {
        $expectedClass     = self::readExpectedException($test);
        $expectedMessage   = self::readPrivateProp($test, 'expectedExceptionMessage');
        $expectedCode      = self::readPrivateProp($test, 'expectedExceptionCode');
        $expectedMsgRegExp = self::readPrivateProp($test, 'expectedExceptionMessageRegExp');

        $anySet = $expectedClass !== null
            || (is_string($expectedMessage) && $expectedMessage !== '')
            || ($expectedCode !== null && $expectedCode !== '')
            || (is_string($expectedMsgRegExp) && $expectedMsgRegExp !== '');
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
        // expectExceptionMessageMatches(): preg_match the thrown message against
        // the user-supplied regex (delimiters are part of the pattern, exactly
        // like vanilla's ExceptionMessageMatchesRegularExpression constraint).
        // An invalid pattern (@preg_match === false) is NOT a match → reported
        // as a non-expected throw, which surfaces the real error to the user.
        if (is_string($expectedMsgRegExp) && $expectedMsgRegExp !== ''
            && @preg_match($expectedMsgRegExp, (string) $thrown->getMessage()) !== 1) {
            return false;
        }
        if ($expectedCode !== null && $expectedCode !== ''
            && (string) $thrown->getCode() !== (string) $expectedCode) {
            return false;
        }
        return true;
    }

    /**
     * P3: assert the test's output expectations against its captured buffer,
     * mirroring vanilla TestCase::performAssertionsOnOutput. Regex takes
     * precedence over the literal-string form (same order as vanilla). Returns
     * a Throwable to record as the test's error on mismatch, or null on
     * pass / when no output expectation was declared.
     */
    private static function assertOutputExpectations(TestCase $test, string $output): ?\Throwable
    {
        $expectedRegex  = self::readPrivateProp($test, 'outputExpectedRegex');
        $expectedString = self::readPrivateProp($test, 'outputExpectedString');

        if (is_string($expectedRegex)) {
            if (@preg_match($expectedRegex, $output) !== 1) {
                return new \PHPUnit\Framework\ExpectationFailedException(sprintf(
                    "Failed asserting that '%s' matches PCRE pattern \"%s\".",
                    $output,
                    $expectedRegex
                ));
            }
            return null;
        }

        if (is_string($expectedString)) {
            if ($output !== $expectedString) {
                return new \PHPUnit\Framework\ExpectationFailedException(sprintf(
                    "Failed asserting that two strings are identical.\n"
                    . "--- Expected\n+++ Actual\n@@ @@\n-'%s'\n+'%s'",
                    $expectedString,
                    $output
                ));
            }
            return null;
        }

        // No output expectation declared: nothing to assert.
        return null;
    }

    /**
     * P6: does the class or method opt into backupGlobals? Method-level wins
     * over class-level. We honor both the PHPUnit 10/11 attribute
     * (#[BackupGlobals(true)]) and the legacy `@backupGlobals enabled` docblock.
     */
    private static function backupGlobalsRequested(\ReflectionClass $ref, string $method): bool
    {
        $methodRef = $ref->hasMethod($method) ? $ref->getMethod($method) : null;

        // Method-level attribute / docblock takes precedence (either direction).
        if ($methodRef !== null) {
            $m = self::readBackupGlobalsFrom($methodRef->getAttributes(), (string) $methodRef->getDocComment());
            if ($m !== null) {
                return $m;
            }
        }
        $c = self::readBackupGlobalsFrom($ref->getAttributes(), (string) $ref->getDocComment());
        return $c ?? false;
    }

    /**
     * Resolve a backupGlobals opt-in from a set of reflection attributes and a
     * docblock. Returns true/false when an explicit opt-in is found at this
     * scope, or null when this scope says nothing (so the caller can fall back).
     *
     * @param \ReflectionAttribute[] $attrs
     */
    private static function readBackupGlobalsFrom(array $attrs, string $doc): ?bool
    {
        foreach ($attrs as $attr) {
            if ($attr->getName() === 'PHPUnit\\Framework\\Attributes\\BackupGlobals') {
                return (bool) $attr->newInstance()->enabled();
            }
        }
        // Legacy docblock: `@backupGlobals enabled` / `@backupGlobals disabled`.
        if ($doc !== '' && preg_match('/@backupGlobals\s+(enabled|disabled|true|false)/i', $doc, $m)) {
            $v = strtolower($m[1]);
            return $v === 'enabled' || $v === 'true';
        }
        return null;
    }

    /**
     * P6: collect the backupGlobalsExcludeList — global variable names that
     * must NOT be snapshotted/restored. PHPUnit 10/11 expresses these via the
     * repeatable #[ExcludeGlobalVariableFromBackup('name')] attribute at class
     * and/or method scope. Returns a flat list of variable names.
     *
     * @return list<string>
     */
    private static function backupGlobalsExcludeList(\ReflectionClass $ref, string $method): array
    {
        $names = [];
        $collect = static function (array $attrs) use (&$names): void {
            foreach ($attrs as $attr) {
                if ($attr->getName() === 'PHPUnit\\Framework\\Attributes\\ExcludeGlobalVariableFromBackup') {
                    $names[] = $attr->newInstance()->globalVariableName();
                }
            }
        };
        $collect($ref->getAttributes());
        if ($ref->hasMethod($method)) {
            $collect($ref->getMethod($method)->getAttributes());
        }
        return array_values(array_unique($names));
    }

    /**
     * P6: snapshot $GLOBALS for backupGlobals. Prefer SebastianBergmann's
     * GlobalState\Snapshot (ships with phpunit) so we match vanilla's exclude
     * semantics exactly; fall back to a plain $GLOBALS array copy when the
     * library is unavailable. Returns an opaque handle consumed by
     * restoreGlobals — either a Snapshot or a ['__plain__' => array] copy.
     *
     * @param list<string> $excludeList
     * @return object|array
     */
    private static function snapshotGlobals(array $excludeList)
    {
        if (class_exists('SebastianBergmann\\GlobalState\\Snapshot')
            && class_exists('SebastianBergmann\\GlobalState\\ExcludeList')) {
            $exclude = new \SebastianBergmann\GlobalState\ExcludeList();
            foreach ($excludeList as $name) {
                $exclude->addGlobalVariable($name);
            }
            // Mirror vanilla createGlobalStateSnapshot: globals on, everything
            // else off (we only back up $GLOBALS here; static properties are
            // handled separately / out of scope — see concerns).
            return new \SebastianBergmann\GlobalState\Snapshot(
                $exclude,
                true,   // includeGlobalVariables
                false,  // includeStaticProperties
                false, false, false, false, false, false, false
            );
        }

        // Fallback: shallow copy of $GLOBALS minus excluded keys. Sufficient for
        // scalar/array markers; matches the documented degraded mode.
        $copy = [];
        foreach ($GLOBALS as $k => $v) {
            if (in_array($k, $excludeList, true)) {
                continue;
            }
            $copy[$k] = $v;
        }
        return ['__plain__' => $copy, '__exclude__' => $excludeList];
    }

    /**
     * P6: restore $GLOBALS from a snapshot taken by snapshotGlobals.
     *
     * @param object|array $backup
     * @param list<string> $excludeList
     */
    private static function restoreGlobals($backup, array $excludeList): void
    {
        if ($backup instanceof \SebastianBergmann\GlobalState\Snapshot) {
            (new \SebastianBergmann\GlobalState\Restorer())->restoreGlobalVariables($backup);
            return;
        }

        // Plain fallback: drop keys added since the snapshot, then overwrite the
        // rest with the saved values. Excluded keys are left untouched.
        if (is_array($backup) && isset($backup['__plain__'])) {
            $saved = $backup['__plain__'];
            foreach (array_keys($GLOBALS) as $k) {
                if ($k === 'GLOBALS' || in_array($k, $excludeList, true)) {
                    continue;
                }
                if (!array_key_exists($k, $saved)) {
                    unset($GLOBALS[$k]);
                }
            }
            foreach ($saved as $k => $v) {
                if ($k === 'GLOBALS') {
                    continue;
                }
                $GLOBALS[$k] = $v;
            }
        }
    }
}
