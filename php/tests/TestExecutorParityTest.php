<?php

declare(strict_types=1);

namespace Proust\Tests;

use Proust\TestExecutor;
use PHPUnit\Framework\Attributes\BackupGlobals;
use PHPUnit\Framework\Attributes\BackupStaticProperties;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\Attributes\ExcludeStaticPropertyFromBackup;
use PHPUnit\Framework\TestCase;

// Holders for the backupStaticProperties parity tests live in the GLOBAL
// namespace (see that file's header) so they are NOT caught by the `Proust`
// static-state exclude that protects the worker runtime — mirroring a real
// project, whose test classes never live under proust's own `Proust\`.
require_once __DIR__ . '/fixtures/backup_static_holders.php';

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

/**
 * Finding (7): #[BackupStaticProperties(true)] must snapshot static class
 * properties before each test and restore them after — the static counterpart
 * of backupGlobals. A test mutates a static; the NEXT test must see the
 * pre-mutation value (the restore rolled it back). This matters acutely in
 * proust's LONG-LIVED worker, where a leaked static persists across every later
 * test in the fork (vanilla's per-test process would hide it). The mutated
 * holders live in the global namespace (fixtures/backup_static_holders.php) to
 * mirror a real project, whose tests are never under proust's own `Proust\`.
 */
#[BackupStaticProperties(true)]
final class _ParityBackupStatic extends TestCase
{
    public function testMutatesStatic(): void
    {
        \_ParityStaticHolder::$counter = 42;
        $this->assertSame(42, \_ParityStaticHolder::$counter);
    }

    public function testStaticWasRestored(): void
    {
        // With backupStaticProperties honored, the previous test's mutation must
        // have been rolled back to the snapshot value (0).
        $this->assertSame(0, \_ParityStaticHolder::$counter);
    }
}

/**
 * Parity guard: WITHOUT the opt-in, static state must LEAK between tests exactly
 * like vanilla (default backupStaticProperties = false). Guards against the
 * feature being accidentally always-on, which would diverge from vanilla and
 * silently slow every suite.
 */
final class _ParityNoBackupStatic extends TestCase
{
    public function testMutates(): void
    {
        \_ParityNoBackupStaticHolder::$counter = 7;
        $this->assertSame(7, \_ParityNoBackupStaticHolder::$counter);
    }

    public function testSeesLeak(): void
    {
        // No opt-in: the mutation is NOT rolled back (vanilla parity).
        $this->assertSame(7, \_ParityNoBackupStaticHolder::$counter);
    }
}

/**
 * Parity: #[ExcludeStaticPropertyFromBackup(class, prop)] removes a property
 * from the snapshot, so it is NOT restored even when backup is on.
 */
#[BackupStaticProperties(true)]
#[ExcludeStaticPropertyFromBackup(\_ParityExcludedStaticHolder::class, 'kept')]
final class _ParityExcludeStatic extends TestCase
{
    public function testMutatesExcluded(): void
    {
        \_ParityExcludedStaticHolder::$kept = 5;          // excluded
        \_ParityExcludedStaticHolder::$alsoBackedUp = 5;  // NOT excluded (control)
        $this->assertSame(5, \_ParityExcludedStaticHolder::$kept);
    }

    public function testExcludedNotRestored(): void
    {
        // The excluded property survives (not rolled back) ...
        $this->assertSame(5, \_ParityExcludedStaticHolder::$kept);
        // ... while the non-excluded control IS rolled back. Asserting both makes
        // this fail if the exclude is ignored (kept -> 0) OR if backup is dead
        // entirely (alsoBackedUp stays 5) — not just the one-sided "leak" half.
        $this->assertSame(0, \_ParityExcludedStaticHolder::$alsoBackedUp);
    }
}

/**
 * Parity: a method-level #[BackupStaticProperties(false)] overrides a
 * class-level true (method scope wins, either direction).
 */
#[BackupStaticProperties(true)]
final class _ParityStaticPrecedence extends TestCase
{
    #[BackupStaticProperties(false)]
    public function testMutatesWithMethodOptOut(): void
    {
        \_ParityStaticPrecedenceHolder::$counter = 9;
        $this->assertSame(9, \_ParityStaticPrecedenceHolder::$counter);
    }

    public function testSeesMethodOptOutLeak(): void
    {
        // The previous method opted OUT (method beats class), so its mutation
        // was NOT rolled back -> still visible here.
        $this->assertSame(9, \_ParityStaticPrecedenceHolder::$counter);
    }
}

/**
 * Worker-safety proof. This holder lives in proust's OWN namespace (Proust\Tests,
 * matching the `Proust` exclude prefix). Even under backupStaticProperties it must
 * be EXCLUDED from the snapshot, so a long-lived worker never has its runtime
 * state (e.g. Proust\SharedTransactionalFixture::$sharedFixtureBuilt) rolled back
 * mid-batch. We prove the exclude is active behaviorally: the mutation must LEAK
 * (not be restored). Remove the `Proust` prefix from globalStateExcludeList() and
 * this test goes red.
 */
final class _ParityProustNamespacedHolder
{
    public static int $counter = 0;
}

#[BackupStaticProperties(true)]
final class _ParityProustExcluded extends TestCase
{
    public function testMutatesProustStatic(): void
    {
        _ParityProustNamespacedHolder::$counter = 13;     // Proust\ -> excluded
        \_ParityProustControlHolder::$counter = 88;        // global -> control
        $this->assertSame(13, _ParityProustNamespacedHolder::$counter);
    }

    public function testProustStaticNotRestored(): void
    {
        // The Proust\-namespaced static is excluded -> NOT rolled back ...
        $this->assertSame(13, _ParityProustNamespacedHolder::$counter);
        // ... while a global control IS rolled back. Asserting both proves the
        // backup machinery is LIVE and that the `Proust\` carve-out is what
        // shields the worker (fails on M1: drop the prefix -> the Proust static
        // rolls back to 0; fails on M2: dead restore -> control stays 88).
        $this->assertSame(0, \_ParityProustControlHolder::$counter);
    }
}

/**
 * Positive method-level opt-in: a class with NO class-level setting but a method
 * carrying #[BackupStaticProperties(true)] must back up. Proves the method-true
 * path of readBackupStaticPropertiesFrom actually triggers a restore (the only
 * method-level attribute elsewhere is `(false)`).
 */
final class _ParityStaticMethodOptIn extends TestCase
{
    #[BackupStaticProperties(true)]
    public function testMutatesOptIn(): void
    {
        \_ParityMethodOptInHolder::$counter = 55;
        $this->assertSame(55, \_ParityMethodOptInHolder::$counter);
    }

    #[BackupStaticProperties(true)]
    public function testOptInRestored(): void
    {
        // Method-level opt-in honored -> previous mutation rolled back to 0.
        $this->assertSame(0, \_ParityMethodOptInHolder::$counter);
    }
}

/**
 * Docblock enable-source: `@backupStaticProperties enabled` (no attribute) must
 * back up exactly like the attribute. Exercises readBackupStaticPropertiesFrom's
 * docblock branch, which is otherwise untested.
 */
/** @backupStaticProperties enabled */
final class _ParityDocblockEnabled extends TestCase
{
    public function testMutatesDocblock(): void
    {
        \_ParityDocblockHolder::$counter = 33;
        $this->assertSame(33, \_ParityDocblockHolder::$counter);
    }

    public function testDocblockRestored(): void
    {
        $this->assertSame(0, \_ParityDocblockHolder::$counter);
    }
}

/**
 * Legacy docblock spelling `@backupStaticAttributes enabled` (PHPUnit < 10) must
 * also enable — the alternation special-cases it.
 */
/** @backupStaticAttributes enabled */
final class _ParityDocblockLegacy extends TestCase
{
    public function testMutatesLegacy(): void
    {
        \_ParityDocblockLegacyHolder::$counter = 22;
        $this->assertSame(22, \_ParityDocblockLegacyHolder::$counter);
    }

    public function testLegacyRestored(): void
    {
        $this->assertSame(0, \_ParityDocblockLegacyHolder::$counter);
    }
}

/**
 * Parity lock for the docblock value semantics: PHPUnit's AnnotationParser turns
 * backup ON only for the case-sensitive literal `enabled`. `@backupStaticProperties
 * true` must therefore be treated as OFF (leak), NOT on. If someone loosens the
 * regex back to accepting `true`, this test goes red.
 */
/** @backupStaticProperties true */
final class _ParityDocblockTrueIsOff extends TestCase
{
    public function testMutatesDocblockTrue(): void
    {
        \_ParityDocblockTrueHolder::$counter = 44;
        $this->assertSame(44, \_ParityDocblockTrueHolder::$counter);
    }

    public function testDocblockTrueLeaks(): void
    {
        // `true` != `enabled` -> backup OFF -> mutation NOT rolled back (vanilla
        // parity: vanilla's stringToBool enables only on `enabled`).
        $this->assertSame(44, \_ParityDocblockTrueHolder::$counter);
    }
}

/**
 * Object static: reassigning a static to a new object must be rolled back to the
 * snapshot value (here null). Restore deep-copies (serialize/unserialize) exactly
 * like vanilla, so this proves reference reassignment is reverted.
 */
#[BackupStaticProperties(true)]
final class _ParityBackupStaticObject extends TestCase
{
    public function testReassignsObject(): void
    {
        \_ParityStaticObjectHolder::$obj = new \stdClass();
        \_ParityStaticObjectHolder::$obj->v = 2;
        $this->assertSame(2, \_ParityStaticObjectHolder::$obj->v);
    }

    public function testObjectReassignmentRestored(): void
    {
        // Snapshot captured null; the reassignment was rolled back.
        $this->assertNull(\_ParityStaticObjectHolder::$obj);
    }
}

/**
 * Combined #[BackupGlobals(true)] + #[BackupStaticProperties(true)] on one class:
 * the two snapshots use disjoint dimensions and restore in order (globals then
 * statics). Both a mutated global and a mutated static must be rolled back.
 */
#[BackupGlobals(true)]
#[BackupStaticProperties(true)]
final class _ParityBackupBoth extends TestCase
{
    public function testMutatesBoth(): void
    {
        $GLOBALS['_parity_both_marker'] = 'x';
        \_ParityBothHolder::$counter = 66;
        $this->assertSame('x', $GLOBALS['_parity_both_marker']);
        $this->assertSame(66, \_ParityBothHolder::$counter);
    }

    public function testBothRestored(): void
    {
        $this->assertArrayNotHasKey('_parity_both_marker', $GLOBALS);
        $this->assertSame(0, \_ParityBothHolder::$counter);
    }
}

/**
 * Worker-hygiene: the restore runs even when the test body THROWS (it sits after
 * the teardown try, reached because the throw is caught into $error). An erroring
 * test must not leak static state into the rest of the long-lived fork.
 */
#[BackupStaticProperties(true)]
final class _ParityBackupStaticThrows extends TestCase
{
    public function testMutatesThenThrows(): void
    {
        \_ParityThrowsHolder::$counter = 77;
        throw new \RuntimeException('boom after mutating a static');
    }

    public function testCounterRestoredAfterThrow(): void
    {
        // Despite the previous test erroring, its static mutation was rolled back.
        $this->assertSame(0, \_ParityThrowsHolder::$counter);
    }
}

/**
 * Method-level #[ExcludeStaticPropertyFromBackup]: backupStaticPropertiesExcludeList
 * reads method scope too, so an exclude declared on the METHOD must carve out that
 * property while a sibling property is still rolled back.
 */
#[BackupStaticProperties(true)]
final class _ParityMethodLevelExclude extends TestCase
{
    #[ExcludeStaticPropertyFromBackup(\_ParityMethodExcludeHolder::class, 'a')]
    public function testMutatesBoth(): void
    {
        \_ParityMethodExcludeHolder::$a = 1;  // excluded at method scope
        \_ParityMethodExcludeHolder::$b = 1;  // not excluded
        $this->assertSame(1, \_ParityMethodExcludeHolder::$a);
    }

    #[ExcludeStaticPropertyFromBackup(\_ParityMethodExcludeHolder::class, 'a')]
    public function testCheckMethodExclusion(): void
    {
        $this->assertSame(1, \_ParityMethodExcludeHolder::$a);  // excluded -> leaked
        $this->assertSame(0, \_ParityMethodExcludeHolder::$b);  // restored
    }
}

/**
 * Isolation parity: an isolated test (#[RunInSeparateProcess]) must SKIP the
 * static backup, mirroring vanilla's snapshotGlobalState early-return when
 * inIsolation — the recycled fork already provides the reset. So even with
 * #[BackupStaticProperties(true)], the mutation must LEAK when run isolated.
 */
#[BackupStaticProperties(true)]
final class _ParityIsolatedBackup extends TestCase
{
    public function testMutatesIsolated(): void
    {
        \_ParityIsolatedHolder::$counter = 71;
        $this->assertSame(71, \_ParityIsolatedHolder::$counter);
    }

    public function testIsolatedLeaks(): void
    {
        // Backup skipped for isolated tests -> mutation NOT rolled back.
        $this->assertSame(71, \_ParityIsolatedHolder::$counter);
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

    // ---- Finding (7): backupStaticProperties opt-in ----

    public function testBackupStaticPropertiesRestoresBetweenTests(): void
    {
        \_ParityStaticHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityBackupStatic::class,
            ['testMutatesStatic', 'testStaticWasRestored']
        );

        $this->assertCount(2, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'second test must see static properties restored to the pre-mutation snapshot: '
            . var_export($outcomes[1], true));
        \_ParityStaticHolder::$counter = 0;
    }

    public function testWithoutOptInStaticStateLeaks(): void
    {
        \_ParityNoBackupStaticHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityNoBackupStatic::class,
            ['testMutates', 'testSeesLeak']
        );
        // Vanilla parity: default off -> no rollback -> both pass (leak expected).
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'], var_export($outcomes[1], true));
        \_ParityNoBackupStaticHolder::$counter = 0;
    }

    public function testExcludeStaticPropertyFromBackupIsHonored(): void
    {
        \_ParityExcludedStaticHolder::$kept = 0;
        $outcomes = TestExecutor::runClass(
            _ParityExcludeStatic::class,
            ['testMutatesExcluded', 'testExcludedNotRestored']
        );
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'an excluded static property must NOT be rolled back: ' . var_export($outcomes[1], true));
        \_ParityExcludedStaticHolder::$kept = 0;
    }

    public function testMethodLevelBackupStaticPropertiesOverridesClass(): void
    {
        \_ParityStaticPrecedenceHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityStaticPrecedence::class,
            ['testMutatesWithMethodOptOut', 'testSeesMethodOptOutLeak']
        );
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'method-level opt-out must beat class-level opt-in (mutation not rolled back): '
            . var_export($outcomes[1], true));
        \_ParityStaticPrecedenceHolder::$counter = 0;
    }

    public function testProustNamespaceStaticsAreExcludedFromBackup(): void
    {
        _ParityProustNamespacedHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityProustExcluded::class,
            ['testMutatesProustStatic', 'testProustStaticNotRestored']
        );
        // The `Proust` exclude shields the worker runtime: the mutation must NOT
        // be rolled back (both pass), proving proust's own static state survives
        // the per-test restore. If the exclude regressed, outcome[1] would fail.
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'a Proust-namespaced static must be excluded from backup (worker safety): '
            . var_export($outcomes[1], true));
        _ParityProustNamespacedHolder::$counter = 0;
        \_ParityProustControlHolder::$counter = 0;
    }

    public function testMethodLevelOptInTriggersRestore(): void
    {
        \_ParityMethodOptInHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityStaticMethodOptIn::class,
            ['testMutatesOptIn', 'testOptInRestored']
        );
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'a method-level #[BackupStaticProperties(true)] must trigger a real restore: '
            . var_export($outcomes[1], true));
        \_ParityMethodOptInHolder::$counter = 0;
    }

    public function testDocblockEnabledIsHonored(): void
    {
        \_ParityDocblockHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityDocblockEnabled::class,
            ['testMutatesDocblock', 'testDocblockRestored']
        );
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            '@backupStaticProperties enabled must back up like the attribute: '
            . var_export($outcomes[1], true));
        \_ParityDocblockHolder::$counter = 0;
    }

    public function testLegacyDocblockSpellingIsHonored(): void
    {
        \_ParityDocblockLegacyHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityDocblockLegacy::class,
            ['testMutatesLegacy', 'testLegacyRestored']
        );
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'legacy @backupStaticAttributes enabled must also back up: '
            . var_export($outcomes[1], true));
        \_ParityDocblockLegacyHolder::$counter = 0;
    }

    public function testDocblockTrueIsTreatedAsOff(): void
    {
        \_ParityDocblockTrueHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityDocblockTrueIsOff::class,
            ['testMutatesDocblockTrue', 'testDocblockTrueLeaks']
        );
        // Parity lock: `@backupStaticProperties true` != `enabled` -> OFF -> leak.
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            '`true` (not `enabled`) must NOT enable backup, matching PHPUnit: '
            . var_export($outcomes[1], true));
        \_ParityDocblockTrueHolder::$counter = 0;
    }

    public function testObjectStaticReassignmentIsRestored(): void
    {
        \_ParityStaticObjectHolder::$obj = null;
        $outcomes = TestExecutor::runClass(
            _ParityBackupStaticObject::class,
            ['testReassignsObject', 'testObjectReassignmentRestored']
        );
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'reassigning a static to a new object must be rolled back: '
            . var_export($outcomes[1], true));
        \_ParityStaticObjectHolder::$obj = null;
    }

    public function testBackupGlobalsAndStaticPropertiesCoexist(): void
    {
        unset($GLOBALS['_parity_both_marker']);
        \_ParityBothHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityBackupBoth::class,
            ['testMutatesBoth', 'testBothRestored']
        );
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'backupGlobals + backupStaticProperties must both restore (disjoint snapshots): '
            . var_export($outcomes[1], true));
        unset($GLOBALS['_parity_both_marker']);
        \_ParityBothHolder::$counter = 0;
    }

    public function testStaticRestoredEvenWhenTestThrows(): void
    {
        \_ParityThrowsHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityBackupStaticThrows::class,
            ['testMutatesThenThrows', 'testCounterRestoredAfterThrow']
        );
        // First test errors (it threw); the restore must STILL have run.
        $this->assertSame('error', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'an erroring test must not leak static state into the fork: '
            . var_export($outcomes[1], true));
        \_ParityThrowsHolder::$counter = 0;
    }

    public function testMethodLevelExcludeStaticPropertyIsHonored(): void
    {
        \_ParityMethodExcludeHolder::$a = 0;
        \_ParityMethodExcludeHolder::$b = 0;
        $outcomes = TestExecutor::runClass(
            _ParityMethodLevelExclude::class,
            ['testMutatesBoth', 'testCheckMethodExclusion']
        );
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'a method-scope #[ExcludeStaticPropertyFromBackup] must carve out only that property: '
            . var_export($outcomes[1], true));
        \_ParityMethodExcludeHolder::$a = 0;
        \_ParityMethodExcludeHolder::$b = 0;
    }

    public function testIsolatedTestsSkipStaticBackup(): void
    {
        \_ParityIsolatedHolder::$counter = 0;
        $outcomes = TestExecutor::runClass(
            _ParityIsolatedBackup::class,
            ['testMutatesIsolated', 'testIsolatedLeaks'],
            null,  // rowFilter
            true   // isolated -> backup skipped (vanilla inIsolation early-return)
        );
        // Both pass: the static leaks because isolated runs skip the snapshot.
        // If the !$isolated gate regressed, backup would restore the static to 0
        // and testIsolatedLeaks (asserting 71) would fail.
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            'isolated tests must skip static backup (the recycled fork resets state): '
            . var_export($outcomes[1], true));
        \_ParityIsolatedHolder::$counter = 0;
    }
}
