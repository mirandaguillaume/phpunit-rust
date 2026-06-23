<?php

declare(strict_types=1);

namespace Proust\Tests;

use Proust\TestExecutor;
use PHPUnit\Framework\TestCase;

/**
 * Fixture exercised THROUGH TestExecutor::runClass (the same path the worker
 * uses). Two tests share one long-lived process. testA inserts a row; testB
 * asserts the table is empty. With the P2 begin/rollback wrapper, testB sees a
 * clean table — proving per-test isolation despite the runBare bypass.
 *
 * The fixture obtains its connection from TestExecutor::connection() — the
 * exact handle the runner wraps in begin/rollback — so testA's INSERT lives
 * inside the runner's transaction and is rolled back before testB runs.
 */
final class _TxFixture extends TestCase
{
    private static function pdo(): \PDO
    {
        // Use the SAME connection the runner wraps in begin/rollback. A local
        // `new \PDO(...)` would be a separate connection that autocommits
        // independently, so the runner's rollback would isolate nothing.
        $pdo = \Proust\TestExecutor::connection();
        \PHPUnit\Framework\Assert::assertNotNull(
            $pdo,
            'TestExecutor::connection() returned null; PHPUNIT_RUST_DB_DSN must be set for this fixture'
        );
        return $pdo;
    }

    public static function setUpBeforeClass(): void
    {
        $pdo = self::pdo();
        // REAL schema. DDL is committed before per-test transactions open.
        $pdo->exec('CREATE TABLE IF NOT EXISTS reset_probe (id INTEGER PRIMARY KEY, val TEXT)');
        $pdo->exec('DELETE FROM reset_probe');
    }

    public function testAInsertsRow(): void
    {
        $pdo = self::pdo();
        $pdo->exec("INSERT INTO reset_probe (id, val) VALUES (1, 'leak')");
        $count = (int) $pdo->query('SELECT COUNT(*) FROM reset_probe')->fetchColumn();
        $this->assertSame(1, $count, 'row visible within its own test');
    }

    public function testBSeesEmptyTable(): void
    {
        $pdo = self::pdo();
        $count = (int) $pdo->query('SELECT COUNT(*) FROM reset_probe')->fetchColumn();
        $this->assertSame(0, $count, "test A's write must be invisible to test B (rollback failed)");
    }
}

/**
 * Finding (5): a test that COMMITS inside the runner's transaction (explicit
 * commit() or a DDL implicit commit) cannot be rolled back — its writes leak
 * into the slot clone for later tests. Full re-clone is out of scope, so the
 * documented mitigation is a loud STDERR breadcrumb. This fixture commits on
 * purpose so we can assert the warning fires.
 */
final class _TxCommitLeakFixture extends TestCase
{
    public function testCommitsInsideTransaction(): void
    {
        $pdo = \Proust\TestExecutor::connection();
        \PHPUnit\Framework\Assert::assertNotNull($pdo);
        // Explicit commit ends the runner-opened transaction. The finally guard
        // sees inTransaction()===false and must emit the leak breadcrumb.
        if ($pdo->inTransaction()) {
            $pdo->commit();
        }
        $this->assertTrue(true);
    }
}

final class TransactionResetTest extends TestCase
{
    private string $dbFile = '';

    protected function setUp(): void
    {
        if (!in_array('sqlite', \PDO::getAvailableDrivers(), true)) {
            $this->markTestSkipped('pdo_sqlite driver not available; P2 isolation test needs a per-worker DB');
        }
        $this->dbFile = sys_get_temp_dir() . '/proust_p2_' . getmypid() . '.sqlite';
        @unlink($this->dbFile);
        putenv('PHPUNIT_RUST_DB_DSN=sqlite:' . $this->dbFile);
    }

    protected function tearDown(): void
    {
        putenv('PHPUNIT_RUST_DB_DSN');
        if ($this->dbFile !== '') {
            @unlink($this->dbFile);
        }
    }

    public function testWritesInTestAreInvisibleToNextTestInSameWorker(): void
    {
        // Run BOTH tests in ORDER, exactly as a worker batch would. Both must
        // pass: A sees its own row, B sees an empty table because A's
        // transaction was rolled back.
        $outcomes = TestExecutor::runClass(
            _TxFixture::class,
            ['testAInsertsRow', 'testBSeesEmptyTable']
        );

        $this->assertCount(2, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status'], var_export($outcomes[0], true));
        $this->assertSame('pass', $outcomes[1]['status'],
            "B must see a clean table; a fail here means the per-test rollback leaked: "
            . var_export($outcomes[1], true));
    }

    public function testCommittedWriteEmitsLeakBreadcrumbToStderr(): void
    {
        // Finding (5): when a test commits inside the runner transaction we can
        // no longer roll it back. We must NOT silently swallow that — a loud
        // STDERR breadcrumb makes the leak visible in forensics. STDERR is
        // process-global and noisy to assert in-process, so drive the fixture
        // in a child PHP process and capture its stderr.
        $autoload = dirname(__DIR__) . '/vendor/autoload.php';
        // _TxCommitLeakFixture is a helper class co-located in THIS file, not a
        // PSR-4 file the composer autoloader can resolve. The fresh child process
        // must require this file to declare it (autoload alone finds the shim +
        // PHPUnit, but not a co-located test helper).
        $php = <<<'PHP'
            require %s;
            require %s;
            putenv('PHPUNIT_RUST_DB_DSN=sqlite:' . %s);
            putenv('PHPUNIT_RUST_SLOT=3');
            \Proust\TestExecutor::runClass(
                \Proust\Tests\_TxCommitLeakFixture::class,
                ['testCommitsInsideTransaction']
            );
            PHP;
        $script = sprintf($php, var_export($autoload, true), var_export(__FILE__, true), var_export($this->dbFile, true));

        $descriptors = [1 => ['pipe', 'w'], 2 => ['pipe', 'w']];
        $proc = proc_open(PHP_BINARY . ' -r ' . escapeshellarg($script), $descriptors, $pipes);
        $this->assertIsResource($proc);
        $stdout = stream_get_contents($pipes[1]);
        $stderr = stream_get_contents($pipes[2]);
        fclose($pipes[1]);
        fclose($pipes[2]);
        proc_close($proc);

        $this->assertStringContainsString('DB isolation LEAK', $stderr,
            "committing inside the txn must emit a leak breadcrumb. stderr=[$stderr] stdout=[$stdout]");
        $this->assertStringContainsString('_TxCommitLeakFixture::testCommitsInsideTransaction', $stderr,
            'the breadcrumb must name the offending class::method');
        $this->assertStringContainsString('slot=3', $stderr,
            'the breadcrumb must name the slot for forensics');
    }
}
