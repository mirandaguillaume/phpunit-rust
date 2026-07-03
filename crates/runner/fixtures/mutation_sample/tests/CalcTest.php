<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\TestCase;
use Sample\Calc;

final class CalcTest extends TestCase
{
    public function testAdd(): void
    {
        self::assertSame(3, (new Calc())->add(1, 2));
    }
}
