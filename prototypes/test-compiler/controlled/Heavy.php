<?php

declare(strict_types=1);

/**
 * A DETERMINISTIC, deliberately-expensive immutable value.
 *
 * `Heavy::build(a, b, c)` does ~1ms of real CPU work (a tight integer mixing loop —
 * no rand, no clock, no I/O), so its result depends only on its arguments. `read()`
 * returns a pre-computed field. The object is read-only: there is no mutator.
 *
 * This is the controlled stand-in for an expensive shared sub-tree: when N test
 * methods all build the SAME Heavy(1,2,3), the e-graph says it is ONE shared
 * e-class of multiplicity N, and a compiler may compute it once.
 */
final class Heavy
{
    private function __construct(public readonly int $value) {}

    public static function build(int $a, int $b, int $c): self
    {
        // Real, deterministic CPU work (~1ms). A 64-bit integer mixing loop seeded
        // only by the arguments — same args ⇒ same result, every time.
        $x = ($a * 1000003) ^ ($b * 19349663) ^ ($c * 83492791);
        for ($i = 0; $i < 600000; $i++) {
            $x ^= ($x << 13) & PHP_INT_MAX;
            $x ^= ($x >> 7);
            $x ^= ($x << 17) & PHP_INT_MAX;
            $x = ($x + 0x9E3779B9) & PHP_INT_MAX;
        }
        return new self($x & 0xFFFF);
    }

    public function read(): int
    {
        return $this->value;
    }
}
