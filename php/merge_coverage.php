<?php

declare(strict_types=1);

/*
 * merge_coverage.php — aggregate per-worker runtime coverage and emit reports.
 *
 * proust drives PHPUnit's own php-code-coverage in each fork-pool worker; every
 * worker flushes its CodeCoverage to a `.cov` file (a `<?php return unserialize(...)`
 * script produced by the library's PHP report writer). After the run the runner
 * invokes this script — mirroring the resource_lease.rs JSON-over-stdio helper —
 * to merge those files with the library's OWN `CodeCoverage::merge()` (the exact
 * mechanism PHPUnit uses to aggregate isolated sub-processes) and write the
 * requested reports with the library's OWN writers, so the output is identical to
 * `phpunit --coverage-*`.
 *
 * Usage:  php merge_coverage.php --autoload <project vendor/autoload.php>
 *   stdin  (JSON): {"files":["…/0-123.cov", …],
 *                   "reports":[{"format":"clover","target":"…/clover.xml"},
 *                              {"format":"text","target":null},
 *                              {"format":"html","target":"…/html-dir"}]}
 *   stdout (JSON): {"ok":true,"written":[…],"text":"…","errors":[…]}
 */

use SebastianBergmann\CodeCoverage\CodeCoverage;
use SebastianBergmann\CodeCoverage\Report\Clover;
use SebastianBergmann\CodeCoverage\Report\Html\Facade as HtmlFacade;
use SebastianBergmann\CodeCoverage\Report\Text;
use SebastianBergmann\CodeCoverage\Report\Thresholds;

$args = [];
for ($i = 1; $i < $argc; $i++) {
    if (str_starts_with($argv[$i], '--') && isset($argv[$i + 1])) {
        $args[substr($argv[$i], 2)] = $argv[++$i];
    }
}
$autoload = $args['autoload'] ?? null;
if ($autoload === null || !is_file($autoload)) {
    fwrite(STDERR, "merge_coverage: --autoload <vendor/autoload.php> required\n");
    echo json_encode(['ok' => false, 'reason' => 'missing autoload']), "\n";
    exit(1);
}
require $autoload;

$request = json_decode((string) stream_get_contents(STDIN), true);
if (!is_array($request)) {
    echo json_encode(['ok' => false, 'reason' => 'bad request json']), "\n";
    exit(1);
}
$files = $request['files'] ?? [];
$reports = $request['reports'] ?? [];

// Aggregate: require each per-worker file (returns a CodeCoverage) and merge.
$acc = null;
$mergeErrors = [];
foreach ($files as $file) {
    if (!is_string($file) || !is_file($file)) {
        continue;
    }
    try {
        $cov = require $file;
    } catch (\Throwable $e) {
        $mergeErrors[] = basename((string) $file) . ': ' . $e->getMessage();
        continue;
    }
    if (!$cov instanceof CodeCoverage) {
        continue;
    }
    if ($acc === null) {
        $acc = $cov;
    } else {
        $acc->merge($cov);
    }
}

if ($acc === null) {
    echo json_encode(['ok' => false, 'reason' => 'no coverage collected', 'errors' => $mergeErrors]), "\n";
    exit(0);
}

// Emit each requested report with the library's own writer. Each is isolated so
// one failing format never loses the others (Clover — the CI target — matters most).
$written = [];
$textOut = null;
$errors = $mergeErrors;
foreach ($reports as $report) {
    $format = $report['format'] ?? '';
    $target = $report['target'] ?? null;
    try {
        switch ($format) {
            case 'clover':
                (new Clover())->process($acc, $target);
                $written[] = $target;
                break;
            case 'html':
                (new HtmlFacade())->process($acc, (string) $target);
                $written[] = $target;
                break;
            case 'text':
                $text = (new Text(Thresholds::default()))->process($acc, false);
                if ($target === null) {
                    $textOut = $text; // returned to the runner, which prints it
                } else {
                    file_put_contents($target, $text);
                    $written[] = $target;
                }
                break;
            default:
                $errors[] = "unknown format: {$format}";
        }
    } catch (\Throwable $e) {
        $errors[] = "{$format}: " . $e->getMessage();
    }
}

echo json_encode(['ok' => true, 'written' => $written, 'text' => $textOut, 'errors' => $errors]), "\n";
