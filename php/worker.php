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

    if (!is_file($autoload)) {
        http_response_code(400);
        echo json_encode(['error' => "autoload not found: {$autoload}"]);
        return;
    }
    if (!isset($loadedAutoloads[$autoload])) {
        require_once $autoload;
        if ($bootstrap !== null && is_file($bootstrap)) {
            require_once $bootstrap;
        }
        $loadedAutoloads[$autoload] = true;
    }
    if (!is_file($file)) {
        http_response_code(400);
        echo json_encode(['error' => "test file not found: {$file}"]);
        return;
    }
    require_once $file;
    if (!class_exists($class)) {
        http_response_code(404);
        echo json_encode(['error' => "class {$class} not found after loading {$file}"]);
        return;
    }

    // Capture and discard any stdout the test or bootstrap prints, so it
    // doesn't corrupt the JSON envelope.
    ob_start();
    try {
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
