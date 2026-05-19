<?php

declare(strict_types=1);

// Suppress PHP 8.5 deprecation noise that would corrupt JSON responses.
error_reporting(E_ALL & ~E_DEPRECATED);

// Tests run arbitrarily long; CLI default time-limit doesn't apply but we
// belt-and-braces it.
@set_time_limit(0);

require_once __DIR__ . '/vendor/autoload.php';

use PhpunitRust\TestExecutor;
use PhpunitRust\MethodPlanner;

// Bind STDIN/STDOUT explicitly so fgets/fwrite work cleanly under any SAPI.
$stdin  = fopen('php://stdin', 'r');
$stdout = fopen('php://stdout', 'w');

$loadedAutoloads = [];

while (($line = fgets($stdin)) !== false) {
    $line = trim($line);
    if ($line === '') continue;

    $req = json_decode($line, true);
    $response = ['error' => 'invalid request'];

    if (!is_array($req)) {
        fwrite($stdout, json_encode($response) . "\n");
        fflush($stdout);
        continue;
    }

    // Required fields.
    $missing = null;
    foreach (['autoload', 'file', 'class', 'methods'] as $required) {
        if (!array_key_exists($required, $req)) { $missing = $required; break; }
    }
    if ($missing !== null) {
        fwrite($stdout, json_encode(['error' => "missing field: $missing"]) . "\n");
        fflush($stdout);
        continue;
    }
    $autoload     = (string) $req['autoload'];
    $file         = (string) $req['file'];
    $class        = (string) $req['class'];
    $methods      = (array)  $req['methods'];
    $bootstrap    = isset($req['bootstrap']) ? (string) $req['bootstrap'] : null;
    $defines      = isset($req['defines']) && is_array($req['defines']) ? $req['defines'] : [];
    $describeOnly = isset($req['describe_only']) && (bool) $req['describe_only'];
    $rowFilter    = isset($req['row_filter']) && is_array($req['row_filter']) ? $req['row_filter'] : null;

    if (!is_file($autoload)) {
        fwrite($stdout, json_encode(['error' => "autoload not found: $autoload"]) . "\n");
        fflush($stdout);
        continue;
    }

    // Capture stdout/stderr during the request so bootstrap echoes don't
    // corrupt our JSON response line. We restore at the end.
    ob_start();
    try {
        if (!isset($loadedAutoloads[$autoload])) {
            require_once $autoload;
            foreach ($defines as $pair) {
                if (is_array($pair) && count($pair) === 2
                    && is_string($pair[0]) && !defined($pair[0])) {
                    define($pair[0], $pair[1]);
                }
            }
            if ($bootstrap !== null && is_file($bootstrap)) {
                require_once $bootstrap;
            }
            // PHPUnit 10+'s MockObject\Invocation::__toString reaches into
            // Registry::get(). Initialize once; PHPUnit 9's different API is
            // covered by the try/catch.
            if (class_exists(\PHPUnit\TextUI\Configuration\Registry::class)) {
                try {
                    \PHPUnit\TextUI\Configuration\Registry::init(
                        (new \PHPUnit\TextUI\CliArguments\Builder)->fromParameters([]),
                        \PHPUnit\TextUI\XmlConfiguration\DefaultConfiguration::create(),
                    );
                } catch (\Throwable) { /* version drift; non-fatal */ }
            }
            $loadedAutoloads[$autoload] = true;
        }
        if (!is_file($file)) {
            ob_end_clean();
            fwrite($stdout, json_encode(['error' => "test file not found: $file"]) . "\n");
            fflush($stdout);
            continue;
        }
        require_once $file;
        if (!class_exists($class)) {
            ob_end_clean();
            fwrite($stdout, json_encode(['error' => "class $class not found after loading $file"]) . "\n");
            fflush($stdout);
            continue;
        }

        if ($describeOnly) {
            // Reflection-only enumeration: methods + depends + (NEW) row counts.
            $ref = new \ReflectionClass($class);
            $description = [];
            foreach ($ref->getMethods(\ReflectionMethod::IS_PUBLIC) as $m) {
                if ($m->getDeclaringClass()->isAbstract()) continue;
                if (!str_starts_with($m->getName(), 'test')) continue;
                $entry = [
                    'name' => $m->getName(),
                    'depends' => MethodPlanner::dependsOf($m),
                ];
                // Row count: call the provider (if any) to count rows.
                // We swallow any exception — provider failures surface at run-time.
                try {
                    $rows = MethodPlanner::dataSetsFor($ref, $m);
                    if ($rows !== null) {
                        $entry['row_count'] = count($rows);
                    }
                } catch (\Throwable) { /* ignore; will surface during the actual run */ }
                $description[] = $entry;
            }
            ob_end_clean();
            fwrite($stdout, json_encode(['description' => $description]) . "\n");
            fflush($stdout);
            continue;
        }

        $outcomes = TestExecutor::runClass($class, $methods, $rowFilter);
        ob_end_clean();
        fwrite($stdout, json_encode(['outcomes' => $outcomes]) . "\n");
        fflush($stdout);
    } catch (\Throwable $e) {
        if (ob_get_level() > 0) {
            ob_end_clean();
        }
        fwrite($stdout, json_encode([
            'error'  => 'worker exception while running class',
            'class'  => $class,
            'detail' => $e->getMessage(),
            'trace'  => $e->getTraceAsString(),
        ]) . "\n");
        fflush($stdout);
    }
}

fclose($stdin);
fclose($stdout);
