<?php

declare(strict_types=1);

/**
 * Per-worker database provisioner (P3). Spawn-once short-lived helper, mirroring
 * enumerate_providers.php's CLI/stdin/stdout contract. The DBMS is an
 * implementation detail behind the Proust\Provisioning\Provisioner contract;
 * the adapter is chosen from the base DSN scheme by ProvisionerFactory.
 *
 * CLI usage:
 *   php provision_db.php --autoload /path/vendor/autoload.php \
 *     [--bootstrap /path/bootstrap.php] [--defines '[["K","V"]]']
 *
 * Reads ONE JSON request on stdin:
 *   {"action":"build_template","base":"<TEMPLATE_DSN_BASE>"}
 *   {"action":"clone","base":...,"template":"app","clone_name":"app_pr1_w0"}
 *   {"action":"drop","base":...,"clone_name":"app_pr1_w0"}
 *   {"action":"gc","base":"<TEMPLATE_DSN_BASE>"}
 *
 * Writes ONE JSON object to stdout: {"dsn":"<conn-string>"} on success (dsn is
 * null for drop/gc), or {"error":"..."} with a non-zero exit on hard failure. The
 * Rust lease gates on the exit code exactly like provider_enum.
 *
 * Adapters: Postgres (`CREATE DATABASE ... TEMPLATE`, base `postgres://user:pass@host:port/db`)
 * and SQLite (file copy, base `sqlite:/abs/path/app.db`). MySQL is the next
 * adapter (needs the per-slot credential channel). Each clone is idempotent and
 * provisioning HARD-FAILS so a required-resource error never degrades the run.
 */

error_reporting(E_ALL & ~E_DEPRECATED);
@set_time_limit(0);

// proust's own autoload (the Provisioning adapters), independent of the project
// under test. Loaded BEFORE the project autoload so the DBMS abstraction is
// always available regardless of the app's classmap.
require_once __DIR__ . '/vendor/autoload.php';

// ---------------------------------------------------------------------------
// 1. Parse CLI args (identical loop to enumerate_providers.php).
// ---------------------------------------------------------------------------
$args = [];
for ($i = 1; $i < $argc; $i++) {
    if (str_starts_with($argv[$i], '--') && isset($argv[$i + 1])) {
        $key = substr($argv[$i], 2);
        $args[$key] = $argv[++$i];
    }
}
$autoload    = $args['autoload']  ?? null;
$bootstrap   = $args['bootstrap'] ?? null;
$definesJson = $args['defines']   ?? '[]';
if ($autoload === null) {
    fwrite(STDERR, "provision_db.php: missing --autoload\n");
    exit(1);
}
$defines = json_decode($definesJson, true) ?? [];

// ---------------------------------------------------------------------------
// 2. Load project autoload + bootstrap (same layering as enumerate_providers.php).
// ---------------------------------------------------------------------------
ob_start();
try {
    require_once $autoload;
    foreach ($defines as $pair) {
        if (is_array($pair) && count($pair) === 2 && is_string($pair[0]) && !defined($pair[0])) {
            define($pair[0], $pair[1]);
        }
    }
} catch (\Throwable $e) {
    if (ob_get_level() > 0) { ob_end_clean(); }
    fwrite(STDERR, "provision_db.php: autoload failed: " . $e->getMessage() . "\n");
    exit(1);
}
if (ob_get_level() > 0) { ob_end_clean(); }
if ($bootstrap !== null && is_file($bootstrap)) {
    ob_start();
    try {
        require_once $bootstrap;
    } catch (\Throwable $e) {
        if (ob_get_level() > 0) { ob_end_clean(); }
        fwrite(STDERR, "provision_db.php: bootstrap failed (continuing): " . $e->getMessage() . "\n");
    }
    if (ob_get_level() > 0) { ob_end_clean(); }
}

// ---------------------------------------------------------------------------
// 3. Read the single JSON request.
// ---------------------------------------------------------------------------
$input = stream_get_contents(STDIN);
$req   = json_decode($input ?: '{}', true);
if (!is_array($req) || !isset($req['action'])) {
    fwrite(STDERR, "provision_db.php: stdin is not a JSON request object\n");
    exit(1);
}
$action = (string) $req['action'];
$base   = (string) ($req['base'] ?? '');
if ($base === '') {
    fwrite(STDERR, "provision_db.php: missing 'base' DSN\n");
    exit(1);
}

try {
    // The DBMS is an implementation detail: the factory picks the adapter from
    // the base DSN scheme; every action below talks only to the Provisioner
    // contract. Postgres / SQLite today; MySQL slots in as another adapter.
    $prov = \Proust\Provisioning\ProvisionerFactory::fromBaseDsn($base);
    switch ($action) {
        case 'build_template':
            // The template IS the base DB/file (already migrated/seeded by the
            // project's bootstrap); cloning copies from it. Return its identity.
            echo json_encode(['dsn' => $prov->templateName()]), "\n";
            exit(0);

        case 'clone':
            $clone = (string) ($req['clone_name'] ?? '');
            if ($clone === '') { throw new \RuntimeException('clone requires clone_name'); }
            echo json_encode(['dsn' => $prov->cloneOne($clone)]), "\n";
            exit(0);

        case 'drop':
            $clone = (string) ($req['clone_name'] ?? '');
            if ($clone === '') { throw new \RuntimeException('drop requires clone_name'); }
            $prov->dropClone($clone);
            echo json_encode(['dsn' => null]), "\n";
            exit(0);

        case 'gc':
            echo json_encode(['dsn' => null, 'dropped' => $prov->gcSweep()]), "\n";
            exit(0);

        // Batched provisioning (the runner's default path): GC sweep + every
        // per-slot clone in ONE invocation. GC is best-effort — a sweep failure
        // (e.g. a concurrent run grabbing a clone) must NOT abort a fresh
        // provision; clone creation below still HARD-FAILS.
        case 'provision_run':
            try {
                $dropped = $prov->gcSweep();
            } catch (\Throwable $e) {
                fwrite(STDERR, "provision_db.php: gc sweep skipped ({$e->getMessage()})\n");
                $dropped = [];
            }
            $dsns = [];
            foreach (($req['clone_names'] ?? []) as $clone) {
                $dsns[] = $prov->cloneOne((string) $clone);
            }
            echo json_encode([
                'template' => $prov->templateName(),
                'dsns'     => $dsns,
                'dropped'  => $dropped,
            ]), "\n";
            exit(0);

        default:
            throw new \RuntimeException("unknown action: $action");
    }
} catch (\Throwable $e) {
    // Hard-fail: write the error and exit non-zero so the Rust lease aborts the
    // run (required-resource build failure must NOT degrade to unprovisioned).
    fwrite(STDERR, 'provision_db.php: ' . $e->getMessage() . "\n");
    echo json_encode(['error' => $e->getMessage()]), "\n";
    exit(1);
}
