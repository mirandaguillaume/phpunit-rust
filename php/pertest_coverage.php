<?php

declare(strict_types=1);

/**
 * Per-test coverage extractor for `proust --mutate`.
 *
 * Reads a JSON request `{"files":["…/0-123.cov", …]}` on stdin — the same
 * delegated per-worker `.cov` files that runtime coverage (#99) already flushes —
 * and writes `{"coverage":{"<abs file>":{"<line>":["Class::method", …]}}}` on
 * stdout. The `.cov` files retain per-test data because `TestExecutor` brackets
 * each test with `$coverage->start("Class::method")`, so we only have to project
 * the library's own `getData()->lineCoverage()` (line → list<testId>) into JSON.
 *
 * Mirrors `php/merge_coverage.php`'s stdin/stdout JSON contract.
 */

require __DIR__ . '/vendor/autoload.php';

$req = json_decode(stream_get_contents(STDIN), true) ?: [];

/** @var array<string, array<int, array<string, true>>> $merged file → line → set(testId) */
$merged = [];

foreach (($req['files'] ?? []) as $f) {
    if (!is_string($f) || !is_file($f)) {
        continue;
    }
    /** @var \SebastianBergmann\CodeCoverage\CodeCoverage $cov */
    $cov = require $f;
    // lineCoverage(): [file => [line => list<testId>]] for covered, executable lines.
    foreach ($cov->getData()->lineCoverage() as $file => $lines) {
        foreach ($lines as $line => $tests) {
            if (!is_array($tests) || $tests === []) {
                continue;
            }
            foreach ($tests as $testId) {
                $merged[$file][$line][$testId] = true;
            }
        }
    }
}

$out = [];
foreach ($merged as $file => $lines) {
    foreach ($lines as $line => $set) {
        $out[$file][(string) $line] = array_keys($set);
    }
}

fwrite(STDOUT, json_encode(['coverage' => $out], JSON_UNESCAPED_SLASHES));
