<?php

declare(strict_types=1);

use PHPUnit\Framework\TestCase;
use SebastianBergmann\CodeCoverage\CodeCoverage;
use SebastianBergmann\CodeCoverage\Driver\Selector;
use SebastianBergmann\CodeCoverage\Filter;
use SebastianBergmann\CodeCoverage\Report\PHP as PhpReport;

/**
 * Verifies `php/pertest_coverage.php` projects a `.cov`'s per-test data into the
 * `{file:{line:[testId]}}` JSON the mutation planner consumes. Needs a line
 * coverage driver (pcov/xdebug) to collect; skipped otherwise (runs in CI Docker).
 */
final class PertestCoverageTest extends TestCase
{
    public function test_reader_maps_lines_to_covering_test_ids(): void
    {
        if (!extension_loaded('pcov') && !extension_loaded('xdebug')) {
            self::markTestSkipped('no line-coverage driver (pcov/xdebug) available');
        }

        // Under the project tree, not /tmp: pcov only instruments files inside its
        // `pcov.directory` scope, which defaults to the project root, not /tmp.
        $dir = __DIR__ . '/pertest_tmp_' . getmypid();
        @mkdir($dir);
        $src = $dir . '/Calc.php';
        // The return is on its own line: pcov only reports clearly executable lines.
        file_put_contents(
            $src,
            "<?php\nclass Calc {\n    public function add(\$a, \$b) {\n        return \$a + \$b;\n    }\n}\n"
        );
        require $src;

        $filter = new Filter();
        $filter->includeFile($src);
        $cov = new CodeCoverage((new Selector())->forLineCoverage($filter), $filter);
        $cov->start('CalcTest::testAdd');
        (new Calc())->add(1, 2);
        $cov->stop();

        $covFile = $dir . '/0.cov';
        (new PhpReport())->process($cov, $covFile);

        $reqFile = $dir . '/req.json';
        file_put_contents($reqFile, json_encode(['files' => [$covFile]]));
        $out = shell_exec(
            'php ' . escapeshellarg(__DIR__ . '/../pertest_coverage.php')
            . ' < ' . escapeshellarg($reqFile)
        );

        $data = json_decode((string) $out, true);
        self::assertArrayHasKey('coverage', $data);
        self::assertArrayHasKey($src, $data['coverage']);
        // Across the covered lines of Calc.php, the bracketing test must appear.
        $allTestIds = array_merge(...array_values($data['coverage'][$src]));
        self::assertContains('CalcTest::testAdd', $allTestIds);

        @unlink($src);
        @unlink($covFile);
        @unlink($reqFile);
        @rmdir($dir);
    }
}
