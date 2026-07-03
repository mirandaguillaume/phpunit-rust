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

    public function ci(string $s)
    {
        return (int) $s;
    }

    public function cf(string $s)
    {
        return (float) $s;
    }

    public function cs(int $n)
    {
        return (string) $n;
    }

    public function cb(int $n)
    {
        return (bool) $n;
    }

    public function ca(int $n)
    {
        return (array) $n;
    }

    public function co(array $a)
    {
        return (object) $a;
    }

    public function expo(int $a, int $b): int
    {
        return $a ** $b;
    }

    public function preinc(int $n): int
    {
        return ++$n;
    }

    public function predec(int $n): int
    {
        return --$n;
    }

    public function five(): int
    {
        return 5;
    }
}
