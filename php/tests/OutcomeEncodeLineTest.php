<?php

declare(strict_types=1);

namespace PhpunitRust\Tests;

use PhpunitRust\OutcomeBuilder;
use PHPUnit\Framework\TestCase;

/**
 * Regression coverage for H3: PHP's json_encode() returns bool(false) — not a
 * string — when any value contains invalid UTF-8 (common in exception messages
 * / stack traces over binary or latin-1 data). The worker wrote results with
 * `fwrite($stream, json_encode($x) . "\n")`, so `false . "\n"` collapsed to a
 * bare "\n" and the TestOutcome line was silently lost → a test-count parity
 * violation.
 *
 * OutcomeBuilder::encodeLine() must ALWAYS return a non-empty, newline-
 * terminated, valid-JSON line that preserves the parseable shape the Rust side
 * (crates/runner WorkerMessage) expects.
 */
final class OutcomeEncodeLineTest extends TestCase
{
    public function testInvalidUtf8MessageStillEncodesValidJson(): void
    {
        // chr(0xB1) is a lone latin-1 byte that is not valid UTF-8. A naive
        // json_encode() of this payload returns false.
        $payload = OutcomeBuilder::build(
            'Some\\Test\\Klass',
            'testThing',
            null,
            1.25,
            new \RuntimeException("boom \xB1 trailing")
        );

        $line = OutcomeBuilder::encodeLine($payload);

        // Must be a real, non-empty line — NOT a bare newline.
        $this->assertNotSame("\n", $line);
        $this->assertStringEndsWith("\n", $line);
        $this->assertGreaterThan(1, strlen($line));

        // Must be parseable JSON (one object per line).
        $decoded = json_decode(rtrim($line, "\n"), true);
        $this->assertNotNull($decoded, 'encoded line did not decode to JSON');
        $this->assertIsArray($decoded);

        // Parseable shape: class/method/status preserved.
        $this->assertSame('Some\\Test\\Klass', $decoded['class']);
        $this->assertSame('testThing', $decoded['method']);
        $this->assertSame('error', $decoded['status']);
        $this->assertArrayHasKey('message', $decoded);
        $this->assertArrayHasKey('trace', $decoded);
        $this->assertArrayHasKey('duration_ms', $decoded);
    }

    public function testCleanPayloadRoundTrips(): void
    {
        $payload = OutcomeBuilder::build('A', 'm', 'ds', 0.5, null);
        $line = OutcomeBuilder::encodeLine($payload);

        $this->assertStringEndsWith("\n", $line);
        $decoded = json_decode(rtrim($line, "\n"), true);
        $this->assertSame('A', $decoded['class']);
        $this->assertSame('m', $decoded['method']);
        $this->assertSame('ds', $decoded['dataset']);
        $this->assertSame('pass', $decoded['status']);
        $this->assertNull($decoded['message']);
    }

    public function testBatchDoneAckEncodes(): void
    {
        $line = OutcomeBuilder::encodeLine(['batch_done' => true]);
        $decoded = json_decode(rtrim($line, "\n"), true);
        $this->assertTrue($decoded['batch_done']);
    }

    public function testInvalidUtf8InNonOutcomePayloadStillEmitsValidLine(): void
    {
        // A payload that is not a test outcome (no class/method) but still has
        // invalid UTF-8: encodeLine must never return a bare newline.
        $line = OutcomeBuilder::encodeLine(['note' => "x\xB1y"]);
        $this->assertNotSame("\n", $line);
        $this->assertStringEndsWith("\n", $line);
        $decoded = json_decode(rtrim($line, "\n"), true);
        $this->assertNotNull($decoded, 'non-outcome line did not decode to JSON');
    }
}
