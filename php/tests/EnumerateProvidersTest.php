<?php

declare(strict_types=1);

namespace PhpunitRust\Tests;

use PHPUnit\Framework\TestCase;

/**
 * Drives the enumerate_providers.php CLI as a subprocess to prove that a single
 * "poison" provider — one whose returned objects throw in __destruct because
 * enumeration never consumes them (php-parser's NodeVisitorForTesting) — does
 * NOT abort the whole enumeration. Before the fix the throwing destructor
 * escaped at script shutdown as an uncatchable fatal (exit 255), and the Rust
 * side discarded the row counts of EVERY provider in the batch (so no provider
 * got split across workers).
 */
final class EnumerateProvidersTest extends TestCase
{
    private function runEnumerator(string $pairsJson): array
    {
        $script   = __DIR__ . '/../enumerate_providers.php';
        $autoload = __DIR__ . '/../vendor/autoload.php';
        $bootstrap = __DIR__ . '/fixtures/enum_poison_fixture.php';

        $cmd = sprintf(
            '%s %s --autoload %s --bootstrap %s',
            escapeshellarg(PHP_BINARY),
            escapeshellarg($script),
            escapeshellarg($autoload),
            escapeshellarg($bootstrap)
        );

        $descriptors = [0 => ['pipe', 'r'], 1 => ['pipe', 'w'], 2 => ['pipe', 'w']];
        $proc = proc_open($cmd, $descriptors, $pipes);
        $this->assertIsResource($proc);
        fwrite($pipes[0], $pairsJson);
        fclose($pipes[0]);
        $stdout = stream_get_contents($pipes[1]);
        $stderr = stream_get_contents($pipes[2]);
        fclose($pipes[1]);
        fclose($pipes[2]);
        $exit = proc_close($proc);

        return ['exit' => $exit, 'stdout' => $stdout, 'stderr' => $stderr];
    }

    public function testPoisonProviderDoesNotWipeOtherCounts(): void
    {
        $pairs = json_encode([
            ['PhpunitRust\\Tests\\Fixtures\\EnumPoison', 'poison'],
            ['PhpunitRust\\Tests\\Fixtures\\EnumPoison', 'good'],
        ]);

        $r = $this->runEnumerator($pairs);

        $this->assertSame(
            0,
            $r['exit'],
            "enumerate_providers must exit 0 despite a poison provider.\nstderr:\n{$r['stderr']}"
        );

        $out = json_decode(trim($r['stdout']), true);
        $this->assertIsArray($out, "stdout must be a JSON object; got: {$r['stdout']}");

        // The good provider's count must survive the poison provider.
        $this->assertSame(
            3,
            $out['PhpunitRust\\Tests\\Fixtures\\EnumPoison::good'] ?? 'MISSING',
            'the plain provider count must not be wiped by the poison one'
        );

        // The poison provider degrades to null (could not be safely enumerated)
        // rather than taking down the batch.
        $this->assertArrayHasKey('PhpunitRust\\Tests\\Fixtures\\EnumPoison::poison', $out);
        $this->assertNull($out['PhpunitRust\\Tests\\Fixtures\\EnumPoison::poison']);
    }

    public function testGatedClassProviderIsNotEnumerated(): void
    {
        // A class gated by an UNMET #[RequiresPhpExtension] is skipped wholesale,
        // so its heavy (20-row) provider must NOT be enumerated → null (= single
        // dispatch unit, no stride-split). Without this, a >=15-row gated method
        // would be split and emit one collapsed skip per chunk on PHPUnit >=10,
        // over-counting vs vanilla's single skip.
        $pairs = json_encode([
            ['PhpunitRust\\Tests\\Fixtures\\EnumGated', 'rows'],
            ['PhpunitRust\\Tests\\Fixtures\\EnumPoison', 'good'],
        ]);

        $r = $this->runEnumerator($pairs);
        $this->assertSame(0, $r['exit'], "enumerate must exit 0.\nstderr:\n{$r['stderr']}");

        $out = json_decode(trim($r['stdout']), true);
        $this->assertIsArray($out, "stdout must be JSON; got: {$r['stdout']}");

        // Gated class → null even though rows() returns 20 entries.
        $this->assertArrayHasKey('PhpunitRust\\Tests\\Fixtures\\EnumGated::rows', $out);
        $this->assertNull(
            $out['PhpunitRust\\Tests\\Fixtures\\EnumGated::rows'],
            "a gated class's provider must not be enumerated (so it is never split)"
        );
        // Sanity: a non-gated provider in the same batch still enumerates.
        $this->assertSame(3, $out['PhpunitRust\\Tests\\Fixtures\\EnumPoison::good'] ?? 'MISSING');
    }
}
