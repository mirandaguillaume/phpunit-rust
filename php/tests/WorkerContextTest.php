<?php

declare(strict_types=1);

namespace Proust\Tests;

use PHPUnit\Framework\TestCase;
use Proust\Worker\WorkerContext;

final class WorkerContextTest extends TestCase
{
    /** @var array<string, string|false> */
    private array $saved = [];

    protected function setUp(): void
    {
        foreach (['PROUST_WORKER_ID', 'PROUST_DB_DSN', 'DATABASE_URL'] as $k) {
            $this->saved[$k] = getenv($k);
            putenv($k);
            unset($_ENV[$k], $_SERVER[$k]);
        }
    }

    protected function tearDown(): void
    {
        foreach ($this->saved as $k => $v) {
            if ($v === false) {
                putenv($k);
                unset($_ENV[$k], $_SERVER[$k]);
            } else {
                WorkerContext::setEnv($k, $v);
            }
        }
    }

    public function testSetEnvIsObservableThroughAllThreeChannels(): void
    {
        WorkerContext::setEnv('PROUST_WORKER_ID', '7');
        $this->assertSame('7', getenv('PROUST_WORKER_ID'));
        $this->assertSame('7', $_ENV['PROUST_WORKER_ID']);
        $this->assertSame('7', $_SERVER['PROUST_WORKER_ID']);
    }

    public function testApplyWithoutDsnSetsOnlyWorkerId(): void
    {
        (new WorkerContext(3, null, false))->apply();
        $this->assertSame('3', getenv('PROUST_WORKER_ID'));
        $this->assertFalse(getenv('PROUST_DB_DSN'));
        $this->assertFalse(getenv('DATABASE_URL'));
    }

    public function testApplyWithDsnButNoEventBridgeSkipsDatabaseUrl(): void
    {
        $dsn = 'pgsql:host=h;port=5432;dbname=app_pr1_w0;user=u;password=p';
        (new WorkerContext(0, $dsn, false))->apply();
        $this->assertSame('0', getenv('PROUST_WORKER_ID'));
        $this->assertSame($dsn, getenv('PROUST_DB_DSN'));
        $this->assertFalse(getenv('DATABASE_URL'), 'marker path must not touch DATABASE_URL');
    }

    public function testApplyWithEventBridgeRepointsDatabaseUrl(): void
    {
        WorkerContext::setEnv('DATABASE_URL', 'postgresql://o:o@db:5432/app?serverVersion=16');
        $dsn = 'pgsql:host=h;port=5432;dbname=app_pr1_w0;user=u;password=p';
        (new WorkerContext(1, $dsn, true))->apply();
        $this->assertSame($dsn, getenv('PROUST_DB_DSN'));
        $this->assertSame(
            'postgresql://u:p@h:5432/app_pr1_w0?serverVersion=16',
            getenv('DATABASE_URL'),
            'event-bridge path repoints DATABASE_URL at the clone, preserving the query'
        );
    }
}
