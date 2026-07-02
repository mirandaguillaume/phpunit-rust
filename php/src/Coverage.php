<?php

declare(strict_types=1);

namespace Proust;

use SebastianBergmann\CodeCoverage\CodeCoverage;
use SebastianBergmann\CodeCoverage\Driver\Selector;
use SebastianBergmann\CodeCoverage\Filter;
use SebastianBergmann\CodeCoverage\Report\PHP as PhpReport;

/**
 * Delegated runtime coverage. proust drives PHPUnit's OWN php-code-coverage
 * library — the same Driver (PCOV/Xdebug), the same Filter, and the same report
 * writers PHPUnit uses internally — so the output is identical to
 * `phpunit --coverage-*`. proust bypasses PHPUnit's runner, so it must start and
 * stop the collector itself (TestExecutor brackets each test), and it aggregates
 * the per-worker results with the library's own `CodeCoverage::merge()` (the same
 * mechanism PHPUnit uses for tests run in isolated sub-processes).
 */
final class Coverage
{
    /**
     * Build a CodeCoverage collector scoped to the phpunit.xml `<source>` config.
     * Returns null (rather than throwing) when php-code-coverage is absent, no
     * `<source>` is configured, or no coverage driver (pcov/xdebug) is loaded —
     * the worker then simply runs without coverage instead of aborting the batch.
     */
    public static function create(?string $configPath): ?CodeCoverage
    {
        if (!class_exists(CodeCoverage::class)) {
            return null;
        }

        $filter = new Filter();
        self::seedFilter($filter, $configPath);
        if ($filter->isEmpty()) {
            return null;
        }

        try {
            $driver = (new Selector())->forLineCoverage($filter);
        } catch (\Throwable) {
            // No pcov/xdebug available -> no runtime coverage.
            return null;
        }

        return new CodeCoverage($driver, $filter);
    }

    /**
     * Persist one worker's coverage to `$file` via the library's PHP report
     * writer (a `return unserialize(...)` script). The merge step `require`s each
     * such file to reconstruct the CodeCoverage object. Best-effort: a failed
     * flush just under-reports that worker; it must never crash shutdown.
     */
    public static function flush(CodeCoverage $coverage, string $file): void
    {
        try {
            (new PhpReport())->process($coverage, $file);
        } catch (\Throwable) {
            // ignore — a lost per-worker file only under-reports.
        }
    }

    /**
     * Seed the Filter (the whitelist that scopes collection) from the config's
     * `<source>` — or the legacy `<coverage>` — include/exclude, matching what
     * PHPUnit reports. Expands directories to their `.php` files via the same
     * `FileIterator` php-code-coverage uses, applies excludes, and hands the net
     * file list to `includeFiles()` (the non-deprecated Filter API). Paths are
     * resolved relative to the config file's directory.
     */
    private static function seedFilter(Filter $filter, ?string $configPath): void
    {
        if ($configPath === null || !is_file($configPath)) {
            return;
        }
        $baseDir = dirname($configPath);
        $xml = @simplexml_load_file($configPath);
        if ($xml === false) {
            return;
        }

        // PHPUnit 10 uses <source>; older suites use <coverage>. Both nest
        // <include>/<exclude> with <directory>/<file> children.
        $include = [];
        $exclude = [];
        foreach (['source', 'coverage'] as $section) {
            if (!isset($xml->{$section})) {
                continue;
            }
            if (isset($xml->{$section}->include)) {
                self::collectFiles($xml->{$section}->include, $baseDir, $include);
            }
            if (isset($xml->{$section}->exclude)) {
                self::collectFiles($xml->{$section}->exclude, $baseDir, $exclude);
            }
        }

        $files = array_values(array_diff($include, $exclude));
        if ($files !== []) {
            $filter->includeFiles($files);
        }
    }

    /**
     * Expand an `<include>`/`<exclude>` node's `<directory>`/`<file>` children to
     * a flat list of absolute file paths, appended to `$out`.
     */
    private static function collectFiles(\SimpleXMLElement $node, string $baseDir, array &$out): void
    {
        $hasIterator = class_exists(\SebastianBergmann\FileIterator\Facade::class);
        foreach ($node->directory as $dir) {
            $path = self::resolve($baseDir, (string) $dir);
            $suffix = isset($dir['suffix']) ? (string) $dir['suffix'] : '.php';
            if (!is_dir($path) || !$hasIterator) {
                continue;
            }
            foreach ((new \SebastianBergmann\FileIterator\Facade())->getFilesAsArray($path, $suffix) as $file) {
                $out[] = $file;
            }
        }
        foreach ($node->file as $file) {
            $path = self::resolve($baseDir, (string) $file);
            if (is_file($path)) {
                $out[] = $path;
            }
        }
    }

    private static function resolve(string $baseDir, string $path): string
    {
        if ($path === '') {
            return $baseDir;
        }
        return ($path[0] === '/') ? $path : $baseDir . '/' . $path;
    }
}
