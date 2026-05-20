<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\TestCase;
use Sample\Calculator;

final class CalculatorTest extends TestCase
{
    public function testAddsTwoPositiveIntegers(): void
    {
        $calc = new Calculator();
        $this->assertSame(5, $calc->add(2, 3));
    }

    public function testAddsNegatives(): void
    {
        $calc = new Calculator();
        $this->assertSame(-7, $calc->add(-3, -4));
    }

    public function testDivisionByZeroThrows(): void
    {
        $calc = new Calculator();
        $this->expectException(\DivisionByZeroError::class);
        $calc->divide(1, 0);
    }
}
