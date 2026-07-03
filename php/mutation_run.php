<?php

declare(strict_types=1);

/**
 * mutation_run.php — warm fork-master for `proust --mutate` (V3).
 *
 * Boots the project ONCE (source-clean: composer autoload is registered but no
 * `<source>`/test class is loaded), then forks a child per mutant. Each child
 * overlays the one mutated file (declaring the mutated class before the autoloader
 * would load the original), runs the mutant's covering tests via `TestExecutor`,
 * and writes its verdict — so the PHP + composer + PHPUnit bootstrap is paid once
 * for the whole run instead of once per mutant (the V2 cold-start this removes).
 *
 * Contract:
 *   php mutation_run.php --autoload <project vendor/autoload.php> \
 *       --jobs <jobs.json> --results <dir> --workers N --timeout S
 * jobs.json: [{ "id": "0", "file": "/tmp/…/mutant.php",
 *               "covering": [{ "class": "Ns\\FooTest", "methods": ["testA","testB"] }] }, …]
 * Each child writes `<dir>/<id>` = "killed" | "escaped" | "timeout". A child that
 * dies without writing (fatal/segfault) is read back as "killed" by the runner.
 */

require __DIR__ . '/vendor/autoload.php'; // Proust\TestExecutor + our PHPUnit shim

use Proust\TestExecutor;

$args = [];
foreach (array_slice($argv, 1) as $a) {
    if (preg_match('/^--([^=]+)=(.*)$/s', $a, $m)) {
        $args[$m[1]] = $m[2];
    }
}
$autoload = $args['autoload'] ?? null;
$bootstrap = $args['bootstrap'] ?? null; // phpunit.xml bootstrap (framework init), if any
$jobsFile = $args['jobs'] ?? null;
$resultsDir = $args['results'] ?? null;
$workers = max(1, (int) ($args['workers'] ?? '4'));
$timeout = max(1, (int) ($args['timeout'] ?? '60'));

if ($autoload === null || $jobsFile === null || $resultsDir === null) {
    fwrite(STDERR, "mutation_run.php: missing --autoload/--jobs/--results\n");
    exit(2);
}
if (!function_exists('pcntl_fork')) {
    fwrite(STDERR, "mutation_run.php: pcntl extension required\n");
    exit(3);
}

// Project bootstrap — mirrors worker_fork.php: register composer's autoloader and
// re-enable PSR-4 fallback (test classes live outside the production classmap).
require_once $autoload;
foreach (spl_autoload_functions() ?: [] as $fn) {
    if (
        is_array($fn) && isset($fn[0]) && is_object($fn[0])
        && $fn[0] instanceof \Composer\Autoload\ClassLoader
        && method_exists($fn[0], 'isClassMapAuthoritative')
        && $fn[0]->isClassMapAuthoritative()
    ) {
        $fn[0]->setClassMapAuthoritative(false);
    }
}
// Run the project's phpunit.xml `bootstrap` ONCE in the source-clean master (framework
// init, constants, …) so every forked child inherits it — this is where an expensive
// framework boot is amortized, and it keeps parity with what phpunit would run. (A
// bootstrap that eagerly loads a class under mutation would defeat the overlay; that is
// out of scope — the oracle gate guards the common autoload-only bootstrap.)
if (is_string($bootstrap) && $bootstrap !== '' && is_file($bootstrap) && realpath($bootstrap) !== realpath($autoload)) {
    require_once $bootstrap;
}

$jobs = json_decode((string) file_get_contents($jobsFile), true) ?: [];
@mkdir($resultsDir, 0777, true);

/** @var array<int,string> $inflight pid => job id */
$inflight = [];

// Reap one finished child. A child that exited without writing its verdict
// (fatal error / crash) counts as a kill — the mutant broke the process.
$reapOne = static function () use (&$inflight, $resultsDir): void {
    $pid = pcntl_waitpid(-1, $status);
    if ($pid <= 0 || !isset($inflight[$pid])) {
        return;
    }
    $id = $inflight[$pid];
    unset($inflight[$pid]);
    if (!is_file("$resultsDir/$id")) {
        file_put_contents("$resultsDir/$id", 'killed');
    }
};

foreach ($jobs as $job) {
    while (count($inflight) >= $workers) {
        $reapOne();
    }
    $pid = pcntl_fork();
    if ($pid === -1) {
        // Fork failed: record a conservative kill and move on.
        file_put_contents("$resultsDir/{$job['id']}", 'killed');
        continue;
    }
    if ($pid === 0) {
        // ---- child ----
        $id = (string) $job['id'];
        pcntl_async_signals(true);
        pcntl_signal(SIGALRM, static function () use ($resultsDir, $id): void {
            file_put_contents("$resultsDir/$id", 'timeout');
            exit(0);
        });
        pcntl_alarm($timeout);

        // Overlay: declare the mutated class before the autoloader loads the original.
        require $job['file'];

        $status = 'escaped';
        foreach (($job['covering'] ?? []) as $cov) {
            try {
                $outcomes = TestExecutor::runClass($cov['class'], $cov['methods']);
            } catch (\Throwable) {
                $status = 'killed';
                break;
            }
            foreach ($outcomes as $o) {
                if (($o['status'] ?? '') === 'fail' || ($o['status'] ?? '') === 'error') {
                    $status = 'killed';
                    break 2;
                }
            }
        }
        file_put_contents("$resultsDir/$id", $status);
        exit(0);
    }
    // ---- parent ----
    $inflight[$pid] = (string) $job['id'];
}

while (!empty($inflight)) {
    $reapOne();
}
