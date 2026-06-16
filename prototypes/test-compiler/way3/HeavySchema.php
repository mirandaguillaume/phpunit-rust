<?php
namespace Cv1;

/**
 * Expensive DETERMINISTIC fixture builder — the "compile a schema / boot an
 * EntityManager" shape. compile() does ~10 ms of pure CPU (no clock/rand/IO),
 * so its result is identical on every call. This is the kind of work a setUp()
 * repeats once per test and that splitting setUp -> setUpBeforeClass hoists to
 * run ONCE — iff the produced object is not mutated by the tests.
 */
final class HeavySchema
{
    public const ITERS = 850000;
    /** @var string[] */ private array $tables;
    private string $fingerprint;

    public static function compile(): self
    {
        $h = 0x1234567;
        for ($i = 0; $i < self::ITERS; $i++) {
            $h = ($h * 1103515245 + 12345) & 0x7fffffff;
            $h ^= ($h >> 7);
        }
        $s = new self();
        $s->fingerprint = sprintf('%08x', $h & 0x7fffffff);
        $s->tables = ['users', 'orders', 'items'];
        return $s;
    }

    public function fingerprint(): string { return $this->fingerprint; }
    /** @return string[] */ public function tables(): array { return $this->tables; }
    public function tableCount(): int { return count($this->tables); }

    // MUTATING method — used only by the negative-control fixture.
    public function addTable(string $t): void { $this->tables[] = $t; }
}
