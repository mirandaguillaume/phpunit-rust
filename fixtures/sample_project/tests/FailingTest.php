<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\TestCase;

final class FailingTest extends TestCase
{
    public function testThisPasses(): void
    {
        $this->assertTrue(true);
    }

    public function testThisDeliberatelyFails(): void
    {
        $this->assertSame(1, 2, 'this is intentional for runner testing');
    }
}
