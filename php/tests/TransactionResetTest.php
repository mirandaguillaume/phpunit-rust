<?php

declare(strict_types=1);

namespace PhpunitRust\Tests;

use PhpunitRust\TestExecutor;
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
        $pdo = \PhpunitRust\TestExecutor::connection();
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

final class TransactionResetTest extends TestCase
{
    private string $dbFile = '';

    protected function setUp(): void
    {
        if (!in_array('sqlite', \PDO::getAvailableDrivers(), true)) {
            $this->markTestSkipped('pdo_sqlite driver not available; P2 isolation test needs a per-worker DB');
        }
        $this->dbFile = sys_get_temp_dir() . '/phpunit_rust_p2_' . getmypid() . '.sqlite';
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
}
