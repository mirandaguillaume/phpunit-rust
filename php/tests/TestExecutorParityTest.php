<?php

declare(strict_types=1);

namespace Proust\Tests;

use Proust\TestExecutor;
use PHPUnit\Framework\Attributes\BackupGlobals;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\TestCase;

/*
 * Parity fixtures + tests for the runBare-bypass findings in TestExecutor.
 * Each fixture isolates ONE vanilla-PHPUnit behavior the bypass got wrong.
 * The fixtures use process-global side-channels (static counters, a public
 * static log) so the test can observe whether a hook actually fired — we
 * cannot read PHPUnit's internal status, only the outcome array TestExecutor
 * returns plus the side effects it left behind.
 */

/**
 * Finding (1): tearDown / #[After] must run even when the test body throws an
 * UNEXPECTED exception, and the ORIGINAL exception must win (vanilla
 * TestCase::runBare runs the teardown sandwich in a separate try and only
 * promotes a teardown throw to the error when the test itself succeeded).
 */
final class _ParityTeardownOnThrow extends TestCase
{
    public static int $tearDownRuns = 0;

    protected function tearDown(): void
    {
        self::$tearDownRuns++;
    }

    public function testBodyThrows(): void
    {
        throw new \RuntimeException('boom from body');
    }
}

/**
 * Finding (1) precedence: if the TEST passes but tearDown throws, the tearDown
 * exception becomes the outcome's error (vanilla: "An exception raised in
 * tearDown() will be caught and passed on when no exception was raised
 * before.").
 */
final class _ParityTeardownThrowsOnPass extends TestCase
{
    protected function tearDown(): void
    {
        throw new \RuntimeException('teardown failure surfaces');
    }

    public function testPasses(): void
    {
        $this->assertTrue(true);
    }
}

/**
 * Finding (2): expectExceptionMessageMatches() (expectedExceptionMessageRegExp)
 * must be honored. A matching throw passes; a non-matching throw errors.
 */
final class _ParityExpectMessageMatches extends TestCase
{
    public function testRegexMatches(): void
    {
        $this->expectExceptionMessageMatches('/co\\w+ failed/');
        throw new \RuntimeException('connection failed');
    }

    public function testRegexNoThrowFails(): void
    {
        $this->expectExceptionMessageMatches('/never/');
        // no throw -> vanilla fails "exception with message matching ... is thrown"
    }

    public function testRegexNonMatchingThrowErrors(): void
    {
        $this->expectException(\RuntimeException::class);
        $this->expectExceptionMessageMatches('/will-not-match/');
        throw new \RuntimeException('completely different message');
    }
}

/**
 * Finding (3): expectOutputString() / expectOutputRegex() must be asserted
 * against the test's own output buffer.
 */
final class _ParityExpectOutput extends TestCase
{
    public function testOutputStringMatches(): void
    {
        $this->expectOutputString('hello world');
        echo 'hello world';
    }

    public function testOutputStringMismatchFails(): void
    {
        $this->expectOutputString('expected');
        echo 'actual';
    }

    public function testOutputRegexMatches(): void
    {
        $this->expectOutputRegex('/\\d+ items/');
        echo '42 items';
    }
}

/**
 * Finding (4): every data-provider row must execute. Two byte-identical rows
 * drive a static counter; the planner may flag the second as a duplicate, but
 * the body must STILL run (state divergence: counter must reach 2).
 */
final class _ParityDuplicateRows extends TestCase
{
    public static int $bodyRuns = 0;

    public static function sameRowTwice(): array
    {
        // Two identical rows -> identical args_hash -> planner is_duplicate.
        return [[7], [7]];
    }

    #[DataProvider('sameRowTwice')]
    public function testCounts(int $n): void
    {
        self::$bodyRuns++;
        $this->assertSame(7, $n);
    }
}

/**
 * Finding (6): #[BackupGlobals(true)] snapshots $GLOBALS before each test and
 * restores after. A test mutates a global; the NEXT test must see the
 * pre-mutation value (here: the global must be absent again).
 */
#[BackupGlobals(true)]
final class _ParityBackupGlobals extends TestCase
{
    public function testMutatesGlobal(): void
    {
        $GLOBALS['_parity_bg_marker'] = 'leaked';
        $this->assertSame('leaked', $GLOBALS['_parity_bg_marker']);
    }

    public function testGlobalWasRestored(): void
    {
        // With backupGlobals honored, the mutation from the previous test must
        // have been rolled back: the key must not be present.
        $this->assertArrayNotHasKey('_parity_bg_marker', $GLOBALS);
    }
}

final class TestExecutorParityTest extends TestCase
{
    // ---- Finding (1): teardown on unexpected throw ----

    public function testTearDownRunsWhenBodyThrows(): void
    {
        _ParityTeardownOnThrow::$tearDownRuns = 0;
        $outcomes = TestExecutor::runClass(_ParityTeardownOnThrow::class, ['testBodyThrows']);

        $this->assertCount(1, $outcomes);
        $this->assertSame('error', $outcomes[0]['status'], var_export($outcomes[0], true));
        // Original exception message must be preserved (not masked by tearDown).
        $this->assertStringContainsString('boom from body', (string) $outcomes[0]['message']);
        // tearDown MUST have executed despite the throw.
        $this->assertSame(1, _ParityTeardownOnThrow::$tearDownRuns,
            'tearDown must run even when the test body throws');
    }

    public function testTearDownThrowSurfacesWhenTestPassed(): void
    {
        $outcomes = TestExecutor::runClass(_ParityTeardownThrowsOnPass::class, ['testPasses']);

        $this->assertCount(1, $outcomes);
        // Test passed but tearDown threw -> outcome is the tearDown error.
        $this->assertSame('error', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertStringContainsString('teardown failure surfaces',
            (string) $outcomes[0]['message']);
    }

    // ---- Finding (2): expectExceptionMessageMatches ----

    public function testExpectMessageMatchesPasses(): void
    {
        $outcomes = TestExecutor::runClass(_ParityExpectMessageMatches::class, ['testRegexMatches']);
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
    }

    public function testExpectMessageMatchesNoThrowFails(): void
    {
        $outcomes = TestExecutor::runClass(_ParityExpectMessageMatches::class, ['testRegexNoThrowFails']);
        // vanilla: AssertionFailedError -> a failure, not a false pass.
        $this->assertContains($outcomes[0]['status'], ['fail', 'error'],
            'a regex message expectation that never throws must NOT pass: '
            . var_export($outcomes[0], true));
    }

    public function testExpectMessageMatchesNonMatchingThrowFails(): void
    {
        $outcomes = TestExecutor::runClass(_ParityExpectMessageMatches::class, ['testRegexNonMatchingThrowErrors']);
        // The class matches but the message regex does not -> must NOT pass.
        $this->assertContains($outcomes[0]['status'], ['fail', 'error'],
            'a non-matching message regex must not be a false pass: '
            . var_export($outcomes[0], true));
    }

    // ---- Finding (3): output expectations ----

    public function testExpectOutputStringMatches(): void
    {
        $outcomes = TestExecutor::runClass(_ParityExpectOutput::class, ['testOutputStringMatches']);
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
    }

    public function testExpectOutputStringMismatchFails(): void
    {
        $outcomes = TestExecutor::runClass(_ParityExpectOutput::class, ['testOutputStringMismatchFails']);
        $this->assertContains($outcomes[0]['status'], ['fail', 'error'],
            'output mismatch must not be a false pass: ' . var_export($outcomes[0], true));
    }

    public function testExpectOutputRegexMatches(): void
    {
        $outcomes = TestExecutor::runClass(_ParityExpectOutput::class, ['testOutputRegexMatches']);
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
    }

    // ---- Finding (4): duplicate rows still execute ----

    public function testDuplicateRowsBothExecute(): void
    {
        _ParityDuplicateRows::$bodyRuns = 0;
        $outcomes = TestExecutor::runClass(_ParityDuplicateRows::class, ['testCounts']);

        $this->assertCount(2, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'], var_export($outcomes[1], true));
        // The body must have run for BOTH rows, not been replayed from cache.
        $this->assertSame(2, _ParityDuplicateRows::$bodyRuns,
            'every data-provider row must execute; no memoized replay');
    }

    // ---- Finding (6): backupGlobals opt-in ----

    public function testBackupGlobalsRestoresBetweenTests(): void
    {
        unset($GLOBALS['_parity_bg_marker']);
        $outcomes = TestExecutor::runClass(
            _ParityBackupGlobals::class,
            ['testMutatesGlobal', 'testGlobalWasRestored']
        );

        $this->assertCount(2, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'second test must see globals restored to the pre-mutation snapshot: '
            . var_export($outcomes[1], true));
        // And no leak into the parent process.
        unset($GLOBALS['_parity_bg_marker']);
    }
}
