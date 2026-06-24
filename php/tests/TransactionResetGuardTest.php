<?php

declare(strict_types=1);

namespace Proust\Tests;

use Proust\TestExecutor;
use PHPUnit\Framework\TestCase;

final class _NoDbFixture extends TestCase
{
    public function testPlain(): void { $this->assertTrue(true); }
}

final class TransactionResetGuardTest extends TestCase
{
    public function testNoDsnMeansNoTransactionAndIdenticalOutcome(): void
    {
        // Ensure the env var is absent for this process.
        putenv('PROUST_DB_DSN');
        unset($_ENV['PROUST_DB_DSN'], $_SERVER['PROUST_DB_DSN']);

        $outcomes = TestExecutor::runClass(_NoDbFixture::class, ['testPlain']);

        $this->assertCount(1, $outcomes);
        $this->assertSame('pass', $outcomes[0]['status']);
        // No exception, no PDO, no 'could not connect' error leaking into the
        // outcome message — the guard short-circuited cleanly.
        $this->assertNull($outcomes[0]['message'] ?? null, var_export($outcomes[0], true));
    }
}
