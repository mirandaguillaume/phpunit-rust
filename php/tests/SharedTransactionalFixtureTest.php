<?php

declare(strict_types=1);

namespace PhpunitRust\Tests;

use PhpunitRust\SharedTransactionalFixture;
use PhpunitRust\TestExecutor;
use PHPUnit\Framework\TestCase;

/**
 * Probe exercised THROUGH TestExecutor::runClass (the worker's exact path). It `use`s the
 * SharedTransactionalFixture trait with counter hooks (no real DB needed), so we can assert
 * the trait's contract directly: buildSharedFixture runs ONCE per class, while
 * beginFixtureTransaction + rollbackFixtureTransaction run once PER test — and every test
 * still passes (the lifecycle does not perturb the body).
 */
final class _SharedFixtureProbe extends TestCase
{
    use SharedTransactionalFixture;

    public static int $built = 0;
    public static int $began = 0;
    public static int $rolled = 0;

    public static function resetCounters(): void
    {
        self::$built  = 0;
        self::$began  = 0;
        self::$rolled = 0;
        self::$sharedFixtureBuilt = []; // reset the per-class build-once guard between probe runs
    }

    protected static function buildSharedFixture(): void
    {
        self::$built++;
    }

    protected static function beginFixtureTransaction(): void
    {
        self::$began++;
    }

    protected static function rollbackFixtureTransaction(): void
    {
        self::$rolled++;
    }

    public function testA(): void
    {
        self::assertSame(1, self::$built, 'fixture built before any test, exactly once');
        self::assertGreaterThanOrEqual(1, self::$began, 'this test opened its transaction in setUp');
    }

    public function testB(): void
    {
        self::assertSame(1, self::$built, 'fixture is NOT rebuilt for the second test');
    }
}

/**
 * The doctrine pattern: an abstract base `use`s the trait and two concrete children extend it.
 * In PHP the children SHARE the base's inherited trait static, so a per-bool guard would let the
 * first child's build suppress the second child's. The guard must key on the CONCRETE class.
 */
abstract class _SharedBaseProbe extends TestCase
{
    use SharedTransactionalFixture;

    /** @var array<class-string,int> build count per concrete class */
    public static array $builtBy = [];

    protected static function buildSharedFixture(): void
    {
        self::$builtBy[static::class] = (self::$builtBy[static::class] ?? 0) + 1;
    }

    protected static function beginFixtureTransaction(): void {}
    protected static function rollbackFixtureTransaction(): void {}
}

final class _ChildAProbe extends _SharedBaseProbe
{
    public function testA(): void { self::assertTrue(true); }
}

final class _ChildBProbe extends _SharedBaseProbe
{
    public function testB(): void { self::assertTrue(true); }
}

final class SharedTransactionalFixtureTest extends TestCase
{
    public function testBuildsOnceAndOpensATransactionPerTest(): void
    {
        _SharedFixtureProbe::resetCounters();

        $outcomes = TestExecutor::runClass(_SharedFixtureProbe::class, ['testA', 'testB']);

        $this->assertCount(2, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'], var_export($outcomes[1], true));

        $this->assertSame(1, _SharedFixtureProbe::$built, 'fixture built EXACTLY once per class');
        $this->assertSame(2, _SharedFixtureProbe::$began, 'exactly one transaction opened per test');
        $this->assertSame(2, _SharedFixtureProbe::$rolled, 'exactly one rollback per test');
    }

    public function testBuildsOnceAcrossMultipleRunClassCalls(): void
    {
        // The runner fragments a class into multiple runClass calls (one per
        // batch/plan). setUpBeforeClass fires per call; the trait's static guard
        // must keep buildSharedFixture to ONCE per worker process regardless.
        _SharedFixtureProbe::resetCounters();

        TestExecutor::runClass(_SharedFixtureProbe::class, ['testA']);
        TestExecutor::runClass(_SharedFixtureProbe::class, ['testB']);

        $this->assertSame(1, _SharedFixtureProbe::$built, 'built once across two runClass calls');
        $this->assertSame(2, _SharedFixtureProbe::$began, 'one transaction per test, both calls');
        $this->assertSame(2, _SharedFixtureProbe::$rolled, 'one rollback per test, both calls');
    }

    public function testEachInheritedClassBuildsIndependently(): void
    {
        // Two concrete children of a trait-using abstract base SHARE the inherited trait
        // static. The build-once guard must key on the CONCRETE class — otherwise the first
        // child's build suppresses the second's (the doctrine abstract-base pattern).
        _SharedBaseProbe::$builtBy = [];

        TestExecutor::runClass(_ChildAProbe::class, ['testA']);
        TestExecutor::runClass(_ChildBProbe::class, ['testB']);

        $this->assertSame(
            1,
            _SharedBaseProbe::$builtBy[_ChildAProbe::class] ?? 0,
            'ChildA built its own fixture once'
        );
        $this->assertSame(
            1,
            _SharedBaseProbe::$builtBy[_ChildBProbe::class] ?? 0,
            'ChildB built its own fixture once (not suppressed by ChildA)'
        );
    }
}
