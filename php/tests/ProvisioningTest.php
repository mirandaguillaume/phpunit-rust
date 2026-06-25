<?php

declare(strict_types=1);

namespace Proust\Tests;

use PHPUnit\Framework\TestCase;
use Proust\Provisioning\DsnUrl;
use Proust\Provisioning\MysqlProvisioner;
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

    public function testFactoryDispatchesMysql(): void
    {
        $this->assertInstanceOf(
            MysqlProvisioner::class,
            ProvisionerFactory::fromBaseDsn('mysql://u:p@h:3306/app')
        );
        $this->assertInstanceOf(
            MysqlProvisioner::class,
            ProvisionerFactory::fromBaseDsn('mariadb://u:p@h/app')
        );
        $this->assertSame(
            'app',
            ProvisionerFactory::fromBaseDsn('mysql://u:p@h/app')->templateName()
        );
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

    public function testDsnUrlPostgresPreservesQuery(): void
    {
        $this->assertSame(
            'postgresql://u:p@h:5432/app_pr1_w0?serverVersion=16&charset=utf8',
            DsnUrl::frameworkUrl(
                'pgsql:host=h;port=5432;dbname=app_pr1_w0;user=u;password=p',
                'postgresql://orig:orig@db:5432/app?serverVersion=16&charset=utf8'
            )
        );
    }

    public function testDsnUrlMysql(): void
    {
        $this->assertSame(
            'mysql://root:root@127.0.0.1:3306/app_pr1_w0',
            DsnUrl::frameworkUrl('mysql:host=127.0.0.1;port=3306;dbname=app_pr1_w0;user=root;password=root', null)
        );
    }

    public function testDsnUrlSqliteAndCredentialEncodingAndUnknown(): void
    {
        $this->assertSame(
            'sqlite:///tmp/app_db_pr1_w0.sqlite',
            DsnUrl::frameworkUrl('sqlite:/tmp/app_db_pr1_w0.sqlite', null)
        );
        // special chars in credentials are percent-encoded
        $this->assertSame(
            'mysql://u%40h:p%3Aw@h:3306/d',
            DsnUrl::frameworkUrl('mysql:host=h;port=3306;dbname=d;user=u@h;password=p:w', null)
        );
        // unknown scheme -> null (caller leaves DATABASE_URL untouched)
        $this->assertNull(DsnUrl::frameworkUrl('oracle:host=h;dbname=d', null));
    }
}
