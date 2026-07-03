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

    public function gt(int $a, int $b): bool
    {
        return $a > $b;
    }

    public function lt(int $a, int $b): bool
    {
        return $a < $b;
    }

    public function lte(int $a, int $b): bool
    {
        return $a <= $b;
    }

    public function eq($a, $b): bool
    {
        return $a == $b;
    }

    public function neq($a, $b): bool
    {
        return $a != $b;
    }

    public function idn($a, $b): bool
    {
        return $a === $b;
    }

    public function nidn($a, $b): bool
    {
        return $a !== $b;
    }

    public function not(bool $x): bool
    {
        return !$x;
    }

    public function one(): float
    {
        return 1.0;
    }

    public function pe(int $n, int $m)
    {
        $n += $m;
        return $n;
    }

    public function me(int $n, int $m)
    {
        $n -= $m;
        return $n;
    }

    public function mule(int $n, int $m)
    {
        $n *= $m;
        return $n;
    }

    public function dive(int $n, int $m)
    {
        $n /= $m;
        return $n;
    }

    public function mode(int $n, int $m)
    {
        $n %= $m;
        return $n;
    }

    public function powe(int $n, int $m)
    {
        $n **= $m;
        return $n;
    }

    public function low(string $s): string
    {
        return strtolower($s);
    }

    public function up(string $s): string
    {
        return strtoupper($s);
    }

    public function tr(string $s): string
    {
        return trim($s);
    }

    public function uf(string $s): string
    {
        return ucfirst($s);
    }

    public function rev(string $s): string
    {
        return strrev($s);
    }

    public function arev(array $a): array
    {
        return array_reverse($a);
    }

    public function auniq(array $a): array
    {
        return array_unique($a);
    }

    public function avals(array $a): array
    {
        return array_values($a);
    }

    public function aflip(array $a): array
    {
        return array_flip($a);
    }

    public function self_()
    {
        return $this;
    }

    public function ship(int $a, int $b): int
    {
        return $a <=> $b;
    }

    public function coal($a, $b)
    {
        return $a ?? $b;
    }

    public function tern(bool $a): string
    {
        return $a ? 'y' : 'n';
    }
}
