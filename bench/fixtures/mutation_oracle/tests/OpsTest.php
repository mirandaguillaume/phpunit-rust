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
}
