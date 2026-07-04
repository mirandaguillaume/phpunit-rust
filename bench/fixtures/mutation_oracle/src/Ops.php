<?php

declare(strict_types=1);

namespace Oracle;

final class Ops
{
    /** @var list<string> */
    private array $log = [];

    public function sum(int $a, int $b): int
    {
        return $a + $b;
    }

    public function bnot(int $x): int
    {
        return ~$x;
    }

    public function run(): array
    {
        $this->record('x');
        return $this->log;
    }

    private function record(string $v): void
    {
        $this->log[] = $v;
    }

    public function runF(array &$out, string $v): void
    {
        array_push($out, $v);
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

    public function cat(string $a, string $b): string
    {
        return $a . $b;
    }

    public function pick(array $items)
    {
        $last = null;
        foreach ($items as $it) {
            $last = $it;
            break;
        }
        return $last;
    }

    public function skipFirst(array $items): array
    {
        $out = [];
        $first = true;
        foreach ($items as $it) {
            if ($first) {
                $first = false;
                continue;
            }
            $out[] = $it;
        }
        return $out;
    }

    public function boom(): void
    {
        throw new \RuntimeException('x');
    }

    public function isRuntime($e): bool
    {
        return $e instanceof \RuntimeException;
    }

    public function sr(string $s): string
    {
        return str_replace('a', 'b', $s);
    }

    public function am(array $a): array
    {
        return array_map('strtoupper', $a);
    }

    public function amrg(array $a, array $b): array
    {
        return array_merge($a, $b);
    }

    public function ifb(bool $c): string
    {
        if ($c) {
            return 'y';
        }
        return 'n';
    }

    public function elifb(bool $a, bool $b): string
    {
        if ($a) {
            return 'a';
        } elseif ($b) {
            return 'b';
        }
        return 'n';
    }

    // The loop fixtures avoid literal 0/1 (seed/step come in as params) so they exercise
    // ONLY the loop mutators — Infection's Number `canMutate` exclusions (skip Increment
    // on `0` under an assignment, etc.) are a separate parity concern, tracked elsewhere.
    public function fsum(array $xs, int $seed): int
    {
        $t = $seed;
        foreach ($xs as $x) {
            $t = $t + $x;
        }
        return $t;
    }

    public function wcount(int $n, int $step): int
    {
        $t = $step;
        $i = $step;
        while ($i < $n) {
            $t = $t + $step;
            $i = $i + $step;
        }
        return $t;
    }

    public function fcount(int $n, int $step): int
    {
        $t = $step;
        for ($i = $step; $i < $n; $i = $i + $step) {
            $t = $t + $step;
        }
        return $t;
    }

    public function dcount(int $n, int $step): int
    {
        $t = $step;
        $i = $step;
        do {
            $t = $t + $step;
            $i = $i + $step;
        } while ($i < $n);
        return $t;
    }

    // --- Number canMutate exclusions (mirror Infection's IncrementInteger/DecrementInteger rules) ---

    public function zeroInit(): int
    {
        // `= 0`: IncrementInteger skipped (0 under assignment); DecrementInteger kept.
        $x = 0;
        return $x + 5;
    }

    public function oneInit(): int
    {
        // `= 1`: DecrementInteger skipped (1 under assignment); IncrementInteger kept.
        $x = 1;
        return $x + 5;
    }

    public function isZero(int $n): bool
    {
        // `=== 0`: IncrementInteger skipped (0 in equality); DecrementInteger kept.
        return $n === 0;
    }

    public function ltFive(int $n): bool
    {
        // `< 5`: BOTH Increment and Decrement skipped (operand of a size comparison).
        return $n < 5;
    }

    public function firstOf(array $a): int
    {
        // `[0]`: DecrementInteger skipped (array zero-index); IncrementInteger kept.
        return $a[0];
    }

    public function dup(\stdClass $o): \stdClass
    {
        // CloneRemoval: `clone $o` -> `$o` returns the same instance.
        return clone $o;
    }

    public function pairs(): \Generator
    {
        // YieldValue: `yield 'k' => 'v'` -> `yield 'v'` drops the key.
        yield 'k' => 'v';
    }

    public function pq(string $s): string
    {
        // PregQuote: `preg_quote($s)` -> `$s` (unwrap arg 0).
        return preg_quote($s);
    }

}
