<?php
namespace Cv1;

/**
 * EXPENSIVE + TIMEZONE-DEPENDENT shared computation.
 *
 * parseExpensive() parses an absolute date string in the AMBIENT default
 * timezone (no zone argument), seeds a deterministic 31-bit mixing loop with the
 * resulting UTC timestamp, and runs ~8 ms of pure CPU work (no clock/rand/IO).
 * The loop is bounded to 31 bits so it never overflows i64 (no float coercion).
 *
 * The result depends on BOTH the input string AND the ambient default timezone
 * at call time: a 5-hour tz offset shifts the seed timestamp, changing the hash.
 * Hoisting the call to a point with a DIFFERENT ambient timezone silently changes
 * the result — exactly the hazard Way-3 must catch statically.
 */
final class Heavy
{
    public const ITERS = 800000;

    public static function parseExpensive(string $str): string
    {
        $dt = new \DateTime($str);              // binds the AMBIENT default timezone
        $h  = $dt->getTimestamp() & 0x7fffffff; // 31-bit seed (2024 ts < 2^31)
        for ($i = 0; $i < self::ITERS; $i++) {
            $h = ($h * 1103515245 + 12345) & 0x7fffffff;
            $h ^= ($h >> 7);
        }
        return sprintf('%08x', $h & 0x7fffffff);
    }
}
