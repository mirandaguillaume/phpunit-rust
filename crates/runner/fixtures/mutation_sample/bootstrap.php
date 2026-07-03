<?php

declare(strict_types=1);

// Tiny PSR-4 autoloader for the fixture, keyed on __DIR__ so a copied-and-mutated
// tree loads ITS OWN src/ (the mutated Calc.php), not the original. PHPUnit's own
// classes come from the phpunit runner's autoloader, not this file.
spl_autoload_register(static function (string $class): void {
    $map = [
        'Sample\\Tests\\' => __DIR__ . '/tests/',
        'Sample\\' => __DIR__ . '/src/',
    ];
    foreach ($map as $prefix => $dir) {
        if (str_starts_with($class, $prefix)) {
            $rel = str_replace('\\', '/', substr($class, strlen($prefix)));
            $file = $dir . $rel . '.php';
            if (is_file($file)) {
                require $file;
            }
            return;
        }
    }
});
