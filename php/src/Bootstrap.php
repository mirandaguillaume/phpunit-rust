<?php

declare(strict_types=1);

namespace PhpunitRust;

use PHPUnit\TestRunner\TestResult\PassedTests;
use PHPUnit\TextUI\CliArguments\Builder as CliBuilder;
use PHPUnit\TextUI\Configuration\PhpHandler;
use PHPUnit\TextUI\Configuration\Registry;
use PHPUnit\TextUI\XmlConfiguration\DefaultConfiguration;
use PHPUnit\TextUI\XmlConfiguration\Loader as XmlLoader;
use PHPUnit\TextUI\Configuration\Configuration;

/**
 * Per-request PHPUnit bootstrap. Builds a Configuration, registers it,
 * applies <php> block and bootstrap file, and resets PHPUnit's hidden
 * singletons so the worker can run thousands of suites cleanly.
 */
final class Bootstrap
{
    /** Keep track of bootstrap files we've already required to avoid double-require errors. */
    private static array $bootstrapsLoaded = [];

    public static function configure(?string $phpunitXmlPath): Configuration
    {
        if ($phpunitXmlPath !== null && is_file($phpunitXmlPath)) {
            $xmlConfig = (new XmlLoader)->load($phpunitXmlPath);
        } else {
            $xmlConfig = DefaultConfiguration::create();
        }
        $cliConfig = (new CliBuilder)->fromParameters([]);
        $config    = Registry::init($cliConfig, $xmlConfig);

        // Apply <php> block (ini settings, env, constants).
        (new PhpHandler)->handle($config->php());

        // Apply the bootstrap file once per worker process.
        if ($config->hasBootstrap()) {
            $path = $config->bootstrap();
            if (!isset(self::$bootstrapsLoaded[$path])) {
                require $path;
                self::$bootstrapsLoaded[$path] = true;
            }
        }

        return $config;
    }

    /**
     * Reset PHPUnit's singletons that would otherwise leak state between
     * worker requests. The most dangerous is PassedTests, which retains
     * @depends-satisfying entries forever.
     */
    public static function resetState(): void
    {
        // PassedTests::$instance is private static — reach in via reflection.
        $ref = new \ReflectionClass(PassedTests::class);
        if ($ref->hasProperty('instance')) {
            $prop = $ref->getProperty('instance');
            $prop->setAccessible(true);
            $prop->setValue(null, null);
        }
    }
}
