<?php

declare(strict_types=1);

use PHPUnit\Framework\TestCase;

require_once __DIR__ . '/Heavy.php';

/**
 * COMPILED: the shared deterministic, immutable sub-tree `Heavy::build(1, 2, 3)`
 * (e-class multiplicity 20) is HOISTED — computed ONCE in setUpBeforeClass into a
 * static memo — and each test references self::$s0 instead of recomputing it.
 */
final class HeavyCompiledTest extends TestCase
{
    private static $s0;

    public static function setUpBeforeClass(): void
    {
        parent::setUpBeforeClass();
        self::$s0 = Heavy::build(1, 2, 3);
    }

    public function test0(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test1(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test2(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test3(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test4(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test5(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test6(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test7(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test8(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test9(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test10(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test11(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test12(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test13(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test14(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test15(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test16(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test17(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test18(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

    public function test19(): void
    {
        $x = self::$s0;
        $this->assertSame(55697, $x->read());
    }

}
