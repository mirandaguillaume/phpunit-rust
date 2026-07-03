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
}
