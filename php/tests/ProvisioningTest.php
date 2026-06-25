<?php

declare(strict_types=1);

namespace Proust\Tests;

use PHPUnit\Framework\TestCase;
use Proust\Provisioning\PgProvisioner;
use Proust\Provisioning\ProvisionerFactory;
use Proust\Provisioning\SqliteProvisioner;

final class ProvisioningTest extends TestCase
{
    public function testFactoryDispatchesByScheme(): void
    {
        $this->assertInstanceOf(
            PgProvisioner::class,
            ProvisionerFactory::fromBaseDsn('postgres://u:p@h:5432/app')
        );
        $this->assertInstanceOf(
            PgProvisioner::class,
            ProvisionerFactory::fromBaseDsn('postgresql://u:p@h/app')
        );
        $this->assertInstanceOf(
            SqliteProvisioner::class,
            ProvisionerFactory::fromBaseDsn('sqlite:/tmp/app.db')
        );
    }

    public function testFactoryRejectsMysqlForNowAndUnknownSchemes(): void
    {
        $this->expectException(\RuntimeException::class);
        $this->expectExceptionMessageMatches('/MySQL/');
        ProvisionerFactory::fromBaseDsn('mysql://u:p@h/app');
    }

    public function testFactoryRejectsUnknownScheme(): void
    {
        $this->expectException(\RuntimeException::class);
        ProvisionerFactory::fromBaseDsn('oracle://u:p@h/app');
    }

    public function testTemplateNameSanitizesForCloneNameMatching(): void
    {
        // pg: the real DB name (already identifier-safe).
        $this->assertSame('app', ProvisionerFactory::fromBaseDsn('postgres://u:p@h/app')->templateName());
        // sqlite: the sanitized file stem — must match the Rust clone-name base
        // (which sanitizes `app.db` -> `app_db`), else the gc pattern misses.
        $this->assertSame('app_db', ProvisionerFactory::fromBaseDsn('sqlite:/tmp/app.db')->templateName());
    }

    public function testSqliteRejectsInMemoryBase(): void
    {
        $this->expectException(\RuntimeException::class);
        new SqliteProvisioner('sqlite::memory:');
    }

    public function testSqliteCloneIsIndependentCopyAndGcReclaims(): void
    {
        if (! \extension_loaded('pdo_sqlite')) {
            $this->markTestSkipped('pdo_sqlite not available');
        }
        $dir = sys_get_temp_dir() . '/proust_prov_' . getmypid() . '_' . uniqid();
        mkdir($dir);
        $tpl = "$dir/app.db";
        $pdo = new \PDO("sqlite:$tpl", null, null, [\PDO::ATTR_ERRMODE => \PDO::ERRMODE_EXCEPTION]);
        $pdo->exec('CREATE TABLE t(id INTEGER)');
        $pdo->exec('INSERT INTO t VALUES (42)');
        $pdo = null;

        $prov = new SqliteProvisioner("sqlite:$tpl");
        $dsn = $prov->cloneOne('app_db_pr1_w0');
        $this->assertSame("sqlite:$dir/app_db_pr1_w0.sqlite", $dsn);

        $clone = new \PDO($dsn, null, null, [\PDO::ATTR_ERRMODE => \PDO::ERRMODE_EXCEPTION]);
        $this->assertSame('42', (string) $clone->query('SELECT id FROM t')->fetchColumn());
        // Write to the clone must NOT leak to the template (isolation).
        $clone->exec('INSERT INTO t VALUES (99)');
        $clone = null;
        $tplCount = (new \PDO("sqlite:$tpl"))->query('SELECT count(*) FROM t')->fetchColumn();
        $this->assertSame('1', (string) $tplCount, 'clone write leaked into the template');

        // gc reclaims the stale clone; dropClone is idempotent.
        $prov->cloneOne('app_db_pr1_w1');
        $dropped = $prov->gcSweep();
        sort($dropped);
        $this->assertSame(['app_db_pr1_w0', 'app_db_pr1_w1'], $dropped);
        $this->assertCount(0, glob("$dir/*.sqlite"));
        $prov->dropClone('app_db_pr1_w0'); // already gone — no error
        $this->addToAssertionCount(1);

        @unlink($tpl);
        @rmdir($dir);
    }

    public function testCloneNameSafetyIsEnforced(): void
    {
        // A clone name that escapes the [A-Za-z0-9_] alphabet (path traversal /
        // SQL injection) is rejected BEFORE any filesystem/DDL action.
        $prov = new SqliteProvisioner('sqlite:/tmp/app.db');
        $this->expectException(\RuntimeException::class);
        $this->expectExceptionMessageMatches('/unsafe/');
        $prov->cloneOne('../../etc/passwd');
    }
}
