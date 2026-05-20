<?php

declare(strict_types=1);

namespace Sample;

final class Calculator
{
    public function add(int $a, int $b): int
    {
        return $a + $b;
    }

    public function divide(int $a, int $b): int
    {
        if ($b === 0) {
            throw new \DivisionByZeroError('cannot divide by zero');
        }
        return intdiv($a, $b);
    }
}
