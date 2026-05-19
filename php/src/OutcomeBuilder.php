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
     * @return array{0:string,1:?string,2:?string}
     */
    private static function classify(?\Throwable $error): array
    {
        if ($error === null) {
            return ['pass', null, null];
        }
        if ($error instanceof SkippedWithMessageException) {
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
