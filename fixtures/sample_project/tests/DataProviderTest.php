<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\TestCase;
use Sample\Calculator;

final class DataProviderTest extends TestCase
{
    public static function additionCases(): array
    {
        return [
            'zeros'    => [0, 0, 0],
            'positive' => [2, 3, 5],
            'negative' => [-1, -1, -2],
            'mixed'    => [10, -3, 7],
        ];
    }

    #[DataProvider('additionCases')]
    public function testAddProducesExpectedSum(int $a, int $b, int $expected): void
    {
        $this->assertSame($expected, (new Calculator())->add($a, $b));
    }
}
