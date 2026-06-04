<?php

declare(strict_types=1);

namespace PhpunitRust\Tests;

use PhpunitRust\TestExecutor;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\Attributes\Depends;
use PHPUnit\Framework\TestCase;

final class _ExecPass extends TestCase
{
    public function testYes(): void { $this->assertTrue(true); }
}

final class _ExecFail extends TestCase
{
    public function testNo(): void { $this->assertSame(1, 2); }
}

final class _ExecExpectException extends TestCase
{
    public function testThrows(): void
    {
        $this->expectException(\RuntimeException::class);
        throw new \RuntimeException('expected');
    }
}

final class _ExecSkipped extends TestCase
{
    public function testSkip(): void { $this->markTestSkipped('nope'); }
    public function testIncomplete(): void { $this->markTestIncomplete('wip'); }
}

final class _ExecProvider extends TestCase
{
    public static function rows(): array { return [[1, 1], [2, 4], [3, 9]]; }
    #[DataProvider('rows')]
    public function testSquare(int $in, int $expected): void
    {
        $this->assertSame($expected, $in * $in);
    }
}

final class _ExecChain extends TestCase
{
    public function testRoot(): array { return ['hello']; }
    #[Depends('testRoot')]
    public function testChild(array $v): void
    {
        $this->assertSame(['hello'], $v);
    }
}

final class _ExecChainVoid extends TestCase
{
    // A dependency that PASSES but returns void (null). PHPUnit treats it as a
    // satisfied dependency regardless of return value and injects null.
    public function testProducer(): void { $this->assertTrue(true); }
    #[Depends('testProducer')]
    public function testConsumer(mixed $fromProducer = null): void
    {
        $this->assertNull($fromProducer);
    }
}

final class _ExecEmptyProvider extends TestCase
{
    // Mirrors faker's localeDataProvider when the locale dirs are absent.
    public static function none(): array { return []; }
    #[DataProvider('none')]
    public function testNoData(int $x): void { $this->fail('must never run'); }
}

final class TestExecutorTest extends TestCase
{
    public function testPassingTestProducesPassOutcome(): void
    {
        $outcomes = TestExecutor::runClass(_ExecPass::class, ['testYes']);
        $this->assertCount(1, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status']);
    }

    public function testFailingAssertionProducesFailOutcome(): void
    {
        $outcomes = TestExecutor::runClass(_ExecFail::class, ['testNo']);
        $this->assertSame('fail', $outcomes[0]['status']);
    }

    public function testExpectExceptionPasses(): void
    {
        $outcomes = TestExecutor::runClass(_ExecExpectException::class, ['testThrows']);
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
    }

    public function testSkippedProducesSkippedOutcome(): void
    {
        $outcomes = TestExecutor::runClass(_ExecSkipped::class, ['testSkip']);
        $this->assertSame('skipped', $outcomes[0]['status']);
        $this->assertSame('nope', $outcomes[0]['message']);
    }

    public function testIncompleteProducesIncompleteOutcome(): void
    {
        $outcomes = TestExecutor::runClass(_ExecSkipped::class, ['testIncomplete']);
        $this->assertSame('incomplete', $outcomes[0]['status']);
    }

    public function testDataProviderExpandsToMultiplePassingOutcomes(): void
    {
        $outcomes = TestExecutor::runClass(_ExecProvider::class, ['testSquare']);
        $this->assertCount(3, $outcomes);
        foreach ($outcomes as $o) {
            $this->assertSame('pass', $o['status'], var_export($o, true));
        }
        $this->assertSame('#0', $outcomes[0]['dataset']);
    }

    public function testDependsInjectsReturnValueAsArg(): void
    {
        $outcomes = TestExecutor::runClass(_ExecChain::class, ['testRoot', 'testChild']);
        $this->assertCount(2, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status']);
        $this->assertSame('pass', $outcomes[1]['status'], var_export($outcomes[1], true));
    }

    public function testEmptyDataProviderYieldsOneSkippedOutcome(): void
    {
        // Regression for faker: a data-provider method that provides NO data is
        // reported by PHPUnit as ONE skipped test, never zero. Without this the
        // method vanished from the count (faker undercounted by the number of
        // empty-provider methods, e.g. ProviderOverrideTest with no locale dirs).
        $outcomes = TestExecutor::runClass(_ExecEmptyProvider::class, ['testNoData']);
        $this->assertCount(1, $outcomes);
        $this->assertSame('skipped', $outcomes[0]['status'], var_export($outcomes[0], true));
    }

    public function testDependsOnVoidReturningDependencyStillRuns(): void
    {
        // Regression: a passing dependency that returns void/null must still
        // satisfy @depends. Previously the dependent skipped with
        // "missing dependency" because the return-value was guarded by
        // `!== null` when recording it. (doctrine-orm: ~40 spurious skips.)
        $outcomes = TestExecutor::runClass(_ExecChainVoid::class, ['testProducer', 'testConsumer']);
        $this->assertCount(2, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status'], 'producer passes');
        $this->assertSame('pass', $outcomes[1]['status'],
            'consumer must run, not skip on a void-returning dependency: '
            . var_export($outcomes[1], true));
    }
}
