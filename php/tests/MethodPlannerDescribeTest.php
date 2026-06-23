<?php

declare(strict_types=1);

namespace Proust\Tests;

use Proust\MethodPlanner;
use PHPUnit\Framework\Attributes\Depends;
use PHPUnit\Framework\TestCase;

final class _MpdSimple extends TestCase
{
    public function testRoot(): int { return 1; }
    #[Depends('testRoot')]
    public function testChild(int $r): int { return $r + 1; }
}

final class _MpdMixed extends TestCase
{
    public function testA(): void {}
    /** @depends testA */
    public function testB(): void {}
    public function testC(): void {}
}

final class MethodPlannerDescribeTest extends TestCase
{
    public function testDependsOfReadsAttribute(): void
    {
        $ref = new \ReflectionClass(_MpdSimple::class);
        $deps = MethodPlanner::dependsOf($ref->getMethod('testChild'));
        $this->assertSame(['testRoot'], $deps);
    }

    public function testDependsOfReadsPhpDoc(): void
    {
        $ref = new \ReflectionClass(_MpdMixed::class);
        $deps = MethodPlanner::dependsOf($ref->getMethod('testB'));
        $this->assertSame(['testA'], $deps);
    }

    public function testDependsOfReturnsEmptyWhenNoneDeclared(): void
    {
        $ref = new \ReflectionClass(_MpdMixed::class);
        $deps = MethodPlanner::dependsOf($ref->getMethod('testC'));
        $this->assertSame([], $deps);
    }
}
