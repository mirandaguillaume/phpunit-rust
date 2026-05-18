<?php

declare(strict_types=1);

require_once __DIR__ . '/vendor/autoload.php';

ignore_user_abort(true);

use PHPUnit\Framework\AssertionFailedError;
use PHPUnit\Framework\ExpectationFailedException;
use PHPUnit\Framework\TestCase;

$loadedProjects = [];

$handler = static function () use (&$loadedProjects): void {
    $raw = file_get_contents('php://input');
    $req = json_decode($raw, true);

    header('Content-Type: application/json');

    if (!is_array($req) || !isset($req['autoload'], $req['file'], $req['class'], $req['method'])) {
        http_response_code(400);
        echo json_encode(['error' => 'missing autoload, file, class, or method']);
        return;
    }

    $autoload = $req['autoload'];
    if (!isset($loadedProjects[$autoload])) {
        if (!is_file($autoload)) {
            http_response_code(400);
            echo json_encode(['error' => "autoload not found: $autoload"]);
            return;
        }
        require_once $autoload;
        $loadedProjects[$autoload] = true;
    }

    if (!is_file($req['file'])) {
        http_response_code(400);
        echo json_encode(['error' => "test file not found: " . $req['file']]);
        return;
    }
    require_once $req['file'];

    $class = $req['class'];
    $method = $req['method'];

    if (!class_exists($class)) {
        http_response_code(404);
        echo json_encode(['error' => "class $class not found after loading " . $req['file']]);
        return;
    }

    if (!is_subclass_of($class, TestCase::class)) {
        http_response_code(400);
        echo json_encode(['error' => "$class does not extend PHPUnit\\Framework\\TestCase"]);
        return;
    }

    $test = new $class($method);

    $status = 'pass';
    $message = null;
    $trace = null;
    $startedAt = microtime(true);

    try {
        Closure::bind(fn () => $this->setUp(), $test, $test)();
        $test->{$method}();
        Closure::bind(fn () => $this->tearDown(), $test, $test)();
    } catch (ExpectationFailedException $e) {
        $status = 'fail';
        $message = $e->getMessage();
        $trace = $e->getTraceAsString();
    } catch (AssertionFailedError $e) {
        $status = 'fail';
        $message = $e->getMessage();
        $trace = $e->getTraceAsString();
    } catch (\Throwable $e) {
        $status = 'error';
        $message = get_class($e) . ': ' . $e->getMessage();
        $trace = $e->getTraceAsString();
    }

    echo json_encode([
        'class' => $class,
        'method' => $method,
        'status' => $status,
        'message' => $message,
        'trace' => $trace,
        'duration_ms' => (microtime(true) - $startedAt) * 1000.0,
    ]);
};

for ($n = 0; $n < 10000; ++$n) {
    $keep = \frankenphp_handle_request($handler);
    gc_collect_cycles();
    if (!$keep) {
        break;
    }
}
