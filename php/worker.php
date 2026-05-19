<?php

declare(strict_types=1);

// Suppress E_DEPRECATED: ReflectionProperty::setAccessible() is a no-op since PHP 8.1
// but still emits deprecation notices in PHP 8.5 that corrupt JSON responses.
error_reporting(E_ALL & ~E_DEPRECATED);

require_once __DIR__ . '/vendor/autoload.php';

use PHPUnit\Framework\TestSuite;
use PHPUnit\Runner\ResultCache\NullResultCache;
use PHPUnit\TextUI\TestRunner;
use PhpunitRust\Bootstrap;
use PhpunitRust\ResultCollector;

ignore_user_abort(true);

// One shared ResultCollector. Subscribers are registered lazily per autoload
// because each project's vendor/autoload.php may bring in its own PHPUnit
// Event\Facade class, creating a different singleton. We must register with
// whichever Facade::instance() is active AFTER the project autoload is loaded.
$collector = new ResultCollector();

// Tracks: autoload path → true (registered with that project's Facade)
$registeredFacades = [];
// Tracks: autoload path → true (loaded)
$loadedAutoloads   = [];

/**
 * Register the collector's typed subscribers with the currently-active
 * PHPUnit\Event\Facade::instance() and flush the DeferringDispatcher so
 * events are dispatched live (not buffered). This must run AFTER the
 * project autoload is required so the correct Facade singleton is used.
 */
function register_collector_with_active_facade(ResultCollector $collector): void
{
    $facade = \PHPUnit\Event\Facade::instance();
    foreach ($collector->subscribers() as $sub) {
        try {
            $facade->registerSubscriber($sub);
        } catch (\PHPUnit\Event\EventFacadeIsSealedException) {
            // Already sealed; we cannot register — this request will return
            // empty outcomes. In a well-configured worker this should not happen.
        }
    }

    // Flush the DeferringDispatcher to switch from recording to live dispatch.
    // We deliberately do NOT call Facade::seal() — sealing would prevent the
    // internal TestResult\Collector (inside TestRunner::run()) from registering
    // its own subscribers. flush() achieves the same recording=false transition.
    $facadeRef = new \ReflectionClass($facade);
    $ddProp    = $facadeRef->getProperty('deferringDispatcher');
    $ddProp->setAccessible(true);
    $dd = $ddProp->getValue($facade);
    if ($dd !== null) {
        $dd->flush();
    }
}

$handler = static function () use ($collector, &$loadedAutoloads, &$registeredFacades): void {
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
    $autoload   = (string) $req['autoload'];
    $file       = (string) $req['file'];
    $class      = (string) $req['class'];
    $methods    = (array)  $req['methods'];
    $phpunitXml = isset($req['phpunit_xml']) ? (string) $req['phpunit_xml'] : null;

    if (!is_file($autoload)) {
        http_response_code(400);
        echo json_encode(['error' => "autoload not found: {$autoload}"]);
        return;
    }
    if (!isset($loadedAutoloads[$autoload])) {
        require_once $autoload;
        $loadedAutoloads[$autoload] = true;
    }

    // After loading the project autoload, register with the active Facade once
    // per unique autoload path (the project may bring its own PHPUnit Facade).
    if (!isset($registeredFacades[$autoload])) {
        register_collector_with_active_facade($collector);
        $registeredFacades[$autoload] = true;
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

    // Capture and discard any stdout the project's bootstrap, autoloader, or
    // tests print. Real-world phpunit.xml bootstraps commonly `echo` config
    // banners ("Using NativeCalculator") before our header() is sent, which
    // would otherwise emit a "headers already sent" warning and corrupt the
    // JSON response body. Buffering here scopes the suppression to the suite
    // run only; field-validation errors above this block are unaffected.
    ob_start();
    try {
        Bootstrap::configure($phpunitXml);
        Bootstrap::resetState();
        $collector->reset();

        $suite = TestSuite::fromClassName($class);

        // Filter to requested methods if a non-empty list was provided.
        // PHPUnit's TestSuite doesn't expose a clean method-set filter, so
        // we walk the tests it produced and drop the non-matching ones in
        // place via reflection on the private $tests property.
        if (!empty($methods)) {
            self_filter_suite_to_methods($suite, $methods);
        }

        (new TestRunner)->run(\PHPUnit\TextUI\Configuration\Registry::get(), new NullResultCache, $suite);
        ob_end_clean();
    } catch (\Throwable $e) {
        if (ob_get_level() > 0) {
            ob_end_clean();
        }
        http_response_code(500);
        echo json_encode([
            'error'   => 'worker exception while running suite',
            'class'   => $class,
            'detail'  => $e->getMessage(),
            'trace'   => $e->getTraceAsString(),
        ]);
        return;
    }

    echo json_encode(['outcomes' => $collector->outcomes()]);
};

/**
 * Restrict a TestSuite to only those tests whose name matches one of the
 * requested method names (data-provider rows are kept if their base method
 * is in the list).
 */
function self_filter_suite_to_methods(TestSuite $suite, array $methodNames): void
{
    $keep = array_flip($methodNames);
    $ref  = new \ReflectionClass($suite);
    $tests = $ref->getProperty('tests');
    $tests->setAccessible(true);
    $current  = $tests->getValue($suite);
    $filtered = [];
    foreach ($current as $test) {
        // $test is a TestCase or another TestSuite (for data providers).
        if ($test instanceof \PHPUnit\Framework\TestCase) {
            if (isset($keep[$test->name()])) {
                $filtered[] = $test;
            }
            continue;
        }
        if ($test instanceof TestSuite) {
            // Data-provider wrapper: name is "ClassName::methodName" — keep
            // the whole sub-suite if its method base is in keep.
            $name       = $test->name();
            $baseMethod = strpos($name, '::') !== false
                ? substr($name, strrpos($name, '::') + 2)
                : $name;
            if (isset($keep[$baseMethod])) {
                $filtered[] = $test;
            }
            continue;
        }
    }
    $tests->setValue($suite, $filtered);
}

for ($n = 0; $n < 10000; ++$n) {
    $keep = \frankenphp_handle_request($handler);
    gc_collect_cycles();
    if (!$keep) {
        break;
    }
}
