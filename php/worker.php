<?php

declare(strict_types=1);

require_once __DIR__ . '/vendor/autoload.php';

ignore_user_abort(true);

$handler = static function (): void {
    $raw = file_get_contents('php://input');
    $req = json_decode($raw, true);

    header('Content-Type: application/json');

    if (!is_array($req)) {
        http_response_code(400);
        echo json_encode(['error' => 'request body must be a JSON object']);
        return;
    }

    echo json_encode([
        'ok' => true,
        'echo' => $req,
        'phpunit_version' => \PHPUnit\Runner\Version::id(),
    ]);
};

for ($nbHandledRequests = 0; $nbHandledRequests < 1000; ++$nbHandledRequests) {
    $keepRunning = \frankenphp_handle_request($handler);
    gc_collect_cycles();
    if (!$keepRunning) {
        break;
    }
}
