<?php

declare(strict_types=1);

namespace PhpunitRust;

use PHPUnit\Framework\AssertionFailedError;
use PHPUnit\Framework\ExpectationFailedException;
use PHPUnit\Framework\IncompleteTestError;
use PHPUnit\Framework\SkippedWithMessageException;

/**
 * Classifies a (possibly null) exception thrown during a test run into the
 * canonical phpunit-rust outcome shape. Pure / side-effect-free.
 */
final class OutcomeBuilder
{
    /**
     * @return array{class:string,method:string,dataset:?string,status:string,message:?string,trace:?string,duration_ms:float}
     */
    public static function build(
        string $class,
        string $method,
        ?string $dataset,
        float $durationMs,
        ?\Throwable $error,
    ): array {
        [$status, $message, $trace] = self::classify($error);
        return [
            'class'       => $class,
            'method'      => $method,
            'dataset'     => $dataset,
            'status'      => $status,
            'message'     => $message,
            'trace'       => $trace,
            'duration_ms' => $durationMs,
        ];
    }

    /**
     * Encode one worker payload as a newline-terminated JSON line that is
     * ALWAYS non-empty and valid JSON — even when the payload contains invalid
     * UTF-8 (common in exception messages / stack traces over binary or
     * latin-1 data).
     *
     * PHP's json_encode() returns bool(false) — not a string — on invalid
     * UTF-8, so the historical `json_encode($x) . "\n"` collapsed to a bare
     * "\n" and the TestOutcome line was silently lost (a test-count parity
     * violation, the project's core correctness goal).
     *
     * Strategy:
     *   1. JSON_INVALID_UTF8_SUBSTITUTE (PHP 7.2+) replaces bad byte sequences
     *      with U+FFFD instead of failing — this handles the overwhelming
     *      majority of cases without losing the payload's structure.
     *   2. JSON_PARTIAL_OUTPUT_ON_ERROR makes json_encode return as much valid
     *      JSON as it can rather than false for the rarer non-UTF-8 failures
     *      (e.g. NAN/INF, malformed UTF-16 surrogate halves, recursion).
     *   3. If json_encode STILL returns false, fall back to a minimal safe
     *      line that preserves the parseable shape the Rust side expects:
     *      class/method/status are kept (so the test is still accounted for),
     *      and message/trace are replaced with a sanitized placeholder noting
     *      the encode failure. The fallback is itself encoded with the same
     *      forgiving flags over ASCII-only data so it cannot fail.
     *
     * @param array<string, mixed> $payload
     */
    public static function encodeLine(array $payload): string
    {
        $flags = JSON_INVALID_UTF8_SUBSTITUTE | JSON_PARTIAL_OUTPUT_ON_ERROR;
        $json = json_encode($payload, $flags);
        if (is_string($json) && $json !== '') {
            return $json . "\n";
        }

        // json_encode failed entirely (returned false or "") despite the
        // forgiving flags. Emit a minimal, shape-preserving fallback so a
        // line is ALWAYS produced and the test is still counted.
        $fallback = self::isOutcomeShape($payload)
            ? [
                'class'       => self::asciiScalar($payload['class'] ?? ''),
                'method'      => self::asciiScalar($payload['method'] ?? ''),
                'dataset'     => null,
                'status'      => self::asciiScalar($payload['status'] ?? 'error'),
                'message'     => '[phpunit-rust: outcome message dropped — json_encode failed (invalid UTF-8 or unencodable value)]',
                'trace'       => null,
                'duration_ms' => is_numeric($payload['duration_ms'] ?? null)
                    ? (float) $payload['duration_ms']
                    : 0.0,
            ]
            : [
                'phpunit_rust_encode_error' => true,
                'message' => '[phpunit-rust: worker payload dropped — json_encode failed (invalid UTF-8 or unencodable value)]',
            ];

        $json = json_encode($fallback, $flags);
        if (!is_string($json) || $json === '') {
            // Absolute last resort: a hand-built constant ASCII line that is
            // guaranteed valid JSON. Should be unreachable.
            $json = '{"phpunit_rust_encode_error":true}';
        }
        return $json . "\n";
    }

    /**
     * A payload is a per-test outcome (vs. a control message such as
     * batch_done / slot_died) when it carries the class+method keys.
     *
     * @param array<string, mixed> $payload
     */
    private static function isOutcomeShape(array $payload): bool
    {
        return array_key_exists('class', $payload) && array_key_exists('method', $payload);
    }

    /**
     * Coerce a value to a string that is guaranteed to be valid UTF-8 (and
     * thus json-encodable) for use in the fallback line. Class/method/status
     * are normally clean ASCII; this guards the pathological case where they
     * are not.
     */
    private static function asciiScalar(mixed $value): string
    {
        if (!is_scalar($value)) {
            return '';
        }
        $s = (string) $value;
        // Drop any byte that is not printable ASCII so the fallback can never
        // itself trip an encode failure.
        return (string) preg_replace('/[^\x20-\x7E]/', '?', $s);
    }

    /**
     * @return array{0:string,1:?string,2:?string}
     */
    private static function classify(?\Throwable $error): array
    {
        if ($error === null) {
            return ['pass', null, null];
        }
        // markTestSkipped throws:
        //   - PHPUnit 10+: SkippedWithMessageException
        //   - PHPUnit 9:   SkippedTestError
        // Detect by class name (fully-qualified) to support both without
        // adding a hard dependency on either symbol.
        $errorClass = get_class($error);
        if (
            $errorClass === 'PHPUnit\\Framework\\SkippedWithMessageException'
            || $errorClass === 'PHPUnit\\Framework\\SkippedTestError'
            || is_subclass_of($error, 'PHPUnit\\Framework\\SkippedTestError')
        ) {
            return ['skipped', $error->getMessage(), null];
        }
        if ($error instanceof IncompleteTestError) {
            return ['incomplete', $error->getMessage(), $error->getTraceAsString()];
        }
        if ($error instanceof ExpectationFailedException || $error instanceof AssertionFailedError) {
            return ['fail', $error->getMessage(), $error->getTraceAsString()];
        }
        $msg = get_class($error) . ': ' . $error->getMessage();
        return ['error', $msg, $error->getTraceAsString()];
    }
}
