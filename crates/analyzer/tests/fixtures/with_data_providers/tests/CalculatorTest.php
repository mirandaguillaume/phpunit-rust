<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;

class CalculatorTest extends TestCase
{
    public static function additionCases(): array
    {
        return [
            'zero_plus_one' => [0, 1, 1],
            'one_plus_one'  => [1, 1, 2],
            'negatives'     => [-1, -1, -2],
        ];
    }

    #[DataProvider('additionCases')]
    public function testAdd(int $a, int $b, int $expected): void
    {
        $calc = new Calculator();
        $result = $calc->add($a, $b);
        $this->assertSame($expected, $result);
    }
}
