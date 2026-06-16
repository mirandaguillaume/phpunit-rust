<?php

declare(strict_types=1);

use PHPUnit\Framework\TestCase;

require_once __DIR__ . '/Heavy.php';

/**
 * ORIGINAL: 20 test methods each independently build the IDENTICAL expensive,
 * deterministic Heavy(1,2,3) and read it. The shared sub-tree is recomputed 20x.
 */
final class HeavyTest extends TestCase
{
    public function test0(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test1(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test2(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test3(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test4(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test5(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test6(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test7(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test8(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test9(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test10(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test11(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test12(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test13(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test14(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test15(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test16(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test17(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test18(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

    public function test19(): void
    {
        $x = Heavy::build(1, 2, 3);
        $this->assertSame(55697, $x->read());
    }

}
