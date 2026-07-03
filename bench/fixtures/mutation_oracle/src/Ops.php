<?php

declare(strict_types=1);

namespace Oracle;

final class Ops
{
    public function sum(int $a, int $b): int
    {
        return $a + $b;
    }

    public function gte(int $a, int $b): bool
    {
        return $a >= $b;
    }

    public function band(int $a, int $b): int
    {
        return $a & $b;
    }

    public function bor(int $a, int $b): int
    {
        return $a | $b;
    }

    public function bxor(int $a, int $b): int
    {
        return $a ^ $b;
    }

    public function shl(int $a, int $b): int
    {
        return $a << $b;
    }

    public function shr(int $a, int $b): int
    {
        return $a >> $b;
    }
}
