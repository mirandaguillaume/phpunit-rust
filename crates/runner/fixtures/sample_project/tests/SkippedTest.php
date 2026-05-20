<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\TestCase;

final class SkippedTest extends TestCase
{
    public function testThatIsExplicitlySkipped(): void
    {
        $this->markTestSkipped('intentionally skipped for runner testing');
    }

    public function testThatIsIncomplete(): void
    {
        $this->markTestIncomplete('not yet finished — intentional for runner testing');
    }

    public function testThatPasses(): void
    {
        $this->assertTrue(true);
    }
}
