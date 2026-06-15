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
}
