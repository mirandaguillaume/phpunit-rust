<?php

declare(strict_types=1);

namespace Oracle\Tests;

use Oracle\Ops;
use PHPUnit\Framework\TestCase;

final class OpsTest extends TestCase
{
    public function testSum(): void
    {
        // `+` -> `-` makes this 3 -> -1: the Plus mutant is KILLED.
        self::assertSame(3, (new Ops())->sum(1, 2));
    }

    public function testGteWhenStrictlyGreater(): void
    {
        // Only the strictly-greater case is exercised, never the equal boundary,
        // so `>=` -> `>` still returns true here: the mutant ESCAPES (both tools).
        self::assertTrue((new Ops())->gte(5, 3));
    }

    public function testBand(): void
    {
        // 6 & 3 = 2; mutated 6 | 3 = 7 -> KILLED.
        self::assertSame(2, (new Ops())->band(6, 3));
    }

    public function testBor(): void
    {
        // 6 | 3 = 7; mutated 6 & 3 = 2 -> KILLED.
        self::assertSame(7, (new Ops())->bor(6, 3));
    }

    public function testBxor(): void
    {
        // 6 ^ 3 = 5; mutated 6 & 3 = 2 -> KILLED.
        self::assertSame(5, (new Ops())->bxor(6, 3));
    }

    public function testShl(): void
    {
        // 1 << 3 = 8; mutated 1 >> 3 = 0 -> KILLED.
        self::assertSame(8, (new Ops())->shl(1, 3));
    }

    public function testShr(): void
    {
        // 8 >> 2 = 2; mutated 8 << 2 = 32 -> KILLED.
        self::assertSame(2, (new Ops())->shr(8, 2));
    }
}
