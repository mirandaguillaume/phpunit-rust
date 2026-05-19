<?php

declare(strict_types=1);

// Suppress PHP 8.5 ReflectionProperty::setAccessible deprecation noise; without
// this, the HTML notice would contaminate JSON responses on any path that
// uses reflection. The calls are no-ops since PHP 8.1 anyway.
error_reporting(E_ALL & ~E_DEPRECATED);

require_once __DIR__ . '/vendor/autoload.php';

use PhpunitRust\TestExecutor;

ignore_user_abort(true);

$loadedAutoloads = [];

$handler = static function () use (&$loadedAutoloads): void {
    // Tests run arbitrarily long; PHPUnit's own CLI disables this.
    set_time_limit(0);

    $raw = file_get_contents('php://input');
    $req = json_decode($raw, true);

    header('Content-Type: application/json');

    foreach (['autoload', 'file', 'class', 'methods'] as $required) {
        if (!is_array($req) || !array_key_exists($required, $req)) {
            http_response_code(400);
            echo json_encode(['error' => "missing field: {$required}"]);
            return;
        }
    }
    $autoload  = (string) $req['autoload'];
    $file      = (string) $req['file'];
    $class     = (string) $req['class'];
    $methods   = (array)  $req['methods'];
    $bootstrap = isset($req['bootstrap']) ? (string) $req['bootstrap'] : null;
    $defines   = isset($req['defines']) && is_array($req['defines']) ? $req['defines'] : [];

    if (!is_file($autoload)) {
        http_response_code(400);
        echo json_encode(['error' => "autoload not found: {$autoload}"]);
        return;
    }

    // Start the output buffer BEFORE any require — autoloaders and bootstrap
    // files commonly echo configuration banners (e.g. brick/math's phpunit.php
    // prints "Using NativeCalculator") which would otherwise corrupt the JSON
    // envelope. Field-validation errors above this point stay outside the
    // buffer so 400s still surface cleanly.
    ob_start();
    try {
        if (!isset($loadedAutoloads[$autoload])) {
            require_once $autoload;
            // Apply phpunit.xml's <php><const .../> declarations BEFORE
            // requiring the bootstrap — many projects' bootstraps read these
            // constants (REQUEST_FACTORY etc. for PSR-17 integration tests).
            foreach ($defines as $pair) {
                if (is_array($pair) && count($pair) === 2
                    && is_string($pair[0]) && !defined($pair[0])) {
                    define($pair[0], $pair[1]);
                }
            }
            if ($bootstrap !== null && is_file($bootstrap)) {
                require_once $bootstrap;
            }
            // PHPUnit 10+'s MockObject\Invocation::__toString eventually calls
            // Registry::get(), which asserts a Configuration is registered.
            // Vanilla PHPUnit's CLI does this; our worker doesn't go through
            // it, so we re-create the minimum init here. PHPUnit 9's API is
            // different (fromParameters takes 2 args; doesn't have the same
            // Registry assertion) — try/catch covers the version drift.
            if (class_exists(\PHPUnit\TextUI\Configuration\Registry::class)) {
                try {
                    \PHPUnit\TextUI\Configuration\Registry::init(
                        (new \PHPUnit\TextUI\CliArguments\Builder)->fromParameters([]),
                        \PHPUnit\TextUI\XmlConfiguration\DefaultConfiguration::create(),
                    );
                } catch (\Throwable) {
                    // PHPUnit 9 or other version drift; Registry init isn't
                    // required there for MockObject error formatting anyway.
                }
            }
            $loadedAutoloads[$autoload] = true;
        }
        if (!is_file($file)) {
            ob_end_clean();
            http_response_code(400);
            echo json_encode(['error' => "test file not found: {$file}"]);
            return;
        }
        require_once $file;
        if (!class_exists($class)) {
            ob_end_clean();
            http_response_code(404);
            echo json_encode(['error' => "class {$class} not found after loading {$file}"]);
            return;
        }
        $outcomes = TestExecutor::runClass($class, $methods);
        ob_end_clean();
        echo json_encode(['outcomes' => $outcomes]);
    } catch (\Throwable $e) {
        if (ob_get_level() > 0) {
            ob_end_clean();
        }
        http_response_code(500);
        echo json_encode([
            'error'  => 'worker exception while running class',
            'class'  => $class,
            'detail' => $e->getMessage(),
            'trace'  => $e->getTraceAsString(),
        ]);
    }
};

for ($n = 0; $n < 10000; ++$n) {
    $keep = \frankenphp_handle_request($handler);
    gc_collect_cycles();
    if (!$keep) {
        break;
    }
}
