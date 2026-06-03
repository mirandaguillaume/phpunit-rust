<?php

declare(strict_types=1);

/**
 * Postgres resource provisioner (P3). Spawn-once short-lived helper, mirroring
 * enumerate_providers.php's CLI/stdin/stdout contract.
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
 * v1 = PostgreSQL only: `CREATE DATABASE ... TEMPLATE` is the cheap storage-
 * level clone primitive. `DROP DATABASE IF EXISTS` makes teardown idempotent.
 * The base DSN must be URL-style: postgres://user:pass@host:port/db
 */

error_reporting(E_ALL & ~E_DEPRECATED);
@set_time_limit(0);

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

/**
 * Connect to the 'postgres' maintenance DB to run CREATE/DROP DATABASE (those
 * cannot run inside a transaction nor against the target DB itself). The base
 * MUST be a URL-style DSN: postgres://user:pass@host:port/db.
 */
function connectAdmin(string $base): \PDO {
    $parts = parse_url($base);
    if ($parts === false || !isset($parts['host'])) {
        throw new \RuntimeException("unparseable --provision-db base DSN (expected postgres://user:pass@host:port/db): $base");
    }
    $host = $parts['host'];
    $port = $parts['port'] ?? 5432;
    $user = $parts['user'] ?? (getenv('PGUSER') ?: 'postgres');
    $pass = $parts['pass'] ?? (getenv('PGPASSWORD') ?: '');
    $dsn  = sprintf('pgsql:host=%s;port=%d;dbname=postgres', $host, $port);
    return new \PDO($dsn, $user, $pass, [\PDO::ATTR_ERRMODE => \PDO::ERRMODE_EXCEPTION]);
}

function dbNameFromBase(string $base): string {
    $path = parse_url($base, PHP_URL_PATH) ?: '';
    return ltrim($path, '/') ?: 'app';
}

function dsnForClone(string $base, string $clone): string {
    // The returned DSN is injected as PHPUNIT_RUST_DB_DSN and consumed by
    // TestExecutor via `new \PDO($dsn)` with NO separate user/pass args, so it
    // MUST be a PDO connection string (pgsql:...) with credentials embedded —
    // not the URL-style form (`postgres://...`), which PDO rejects with
    // "could not find driver".
    $parts = parse_url($base);
    $host  = $parts['host'] ?? '127.0.0.1';
    $port  = $parts['port'] ?? 5432;
    $user  = $parts['user'] ?? (getenv('PGUSER') ?: 'postgres');
    $pass  = $parts['pass'] ?? (getenv('PGPASSWORD') ?: '');
    return sprintf(
        'pgsql:host=%s;port=%d;dbname=%s;user=%s;password=%s',
        $host, $port, $clone, $user, $pass
    );
}

/** Quote an identifier for Postgres DDL (double-quote, escape embedded quotes). */
function qid(string $id): string { return '"' . str_replace('"', '""', $id) . '"'; }

/**
 * Defense-in-depth: every identifier interpolated into DDL must already be
 * sanitized by the Rust lease (clone_name maps to [A-Za-z0-9_] and bounds to
 * 63 bytes). Reject anything else hard, BEFORE running DDL, so a malformed
 * request can never reach the database. We keep qid() too (belt + braces).
 */
function assertSafeIdent(string $id, string $what): void {
    if ($id === '' || strlen($id) > 63 || !preg_match('/^[A-Za-z0-9_]+$/', $id)) {
        throw new \RuntimeException("unsafe $what identifier (expected ^[A-Za-z0-9_]+\$, <=63 bytes): $id");
    }
}

try {
    $pdo = connectAdmin($base);
    switch ($action) {
        case 'build_template':
            // The template is the base DB itself (already migrated/seeded by the
            // project's bootstrap). We return its name; cloning copies from it.
            $tpl = dbNameFromBase($base);
            echo json_encode(['dsn' => $tpl]), "\n";
            exit(0);

        case 'clone':
            $template = (string) ($req['template'] ?? dbNameFromBase($base));
            $clone    = (string) ($req['clone_name'] ?? '');
            if ($clone === '') { throw new \RuntimeException('clone requires clone_name'); }
            assertSafeIdent($clone, 'clone_name');
            assertSafeIdent($template, 'template');
            // Idempotent: drop any stale clone from a previous crashed run first.
            $pdo->exec('DROP DATABASE IF EXISTS ' . qid($clone));
            $pdo->exec('CREATE DATABASE ' . qid($clone) . ' TEMPLATE ' . qid($template));
            echo json_encode(['dsn' => dsnForClone($base, $clone)]), "\n";
            exit(0);

        case 'drop':
            $clone = (string) ($req['clone_name'] ?? '');
            if ($clone === '') { throw new \RuntimeException('drop requires clone_name'); }
            assertSafeIdent($clone, 'clone_name');
            // Terminate any lingering backends so DROP succeeds even if a
            // SIGKILLed worker left a connection open, then DROP IF EXISTS.
            $stmt = $pdo->prepare(
                'SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = ?'
            );
            $stmt->execute([$clone]);
            $pdo->exec('DROP DATABASE IF EXISTS ' . qid($clone));
            echo json_encode(['dsn' => null]), "\n";
            exit(0);

        /**
         * Best-effort crash cleanup: drop clone databases left by a prior run
         * that was killed before its LeaseGuard could run destroy_all().
         *
         * Safety rule: only drop clones with ZERO active backends. A non-zero
         * count means a live concurrent run owns that clone — we must not touch
         * it. This relies on the assumption that no two runs provision against
         * the SAME base DSN concurrently (use isolated base DBs for that; CI
         * service containers give each job its own Postgres instance).
         *
         * Match exactly `<base>_pr<digits>_w<digits>` with a POSIX regex (the `~`
         * operator) — precise, and free of the LIKE ESCAPE-string pitfalls (a
         * malformed multi-char escape errors the whole sweep out). Clones the
         * Rust side hash-splices for >63-byte base names (rare) won't match and
         * are left for a later run; the common short-name case is covered.
         */
        case 'gc':
            $baseName = dbNameFromBase($base);
            // Escape POSIX-ERE metacharacters in the base name; the clone suffix
            // is the fixed shape `_pr<digits>_w<digits>`.
            $reBase = preg_replace('/[.^$*+?()\\[\\]{}|\\\\]/', '\\\\$0', $baseName);
            $pat = '^' . $reBase . '_pr[0-9]+_w[0-9]+$';
            $stmt = $pdo->prepare(
                'SELECT datname FROM pg_database WHERE datname ~ :pat AND datname <> :base'
            );
            $stmt->execute([':pat' => $pat, ':base' => $baseName]);
            $candidates = $stmt->fetchAll(\PDO::FETCH_COLUMN);

            $dropped = [];
            foreach ($candidates as $name) {
                // Count active backends — skip any clone still in use.
                $bStmt = $pdo->prepare(
                    'SELECT count(*) FROM pg_stat_activity WHERE datname = ?'
                );
                $bStmt->execute([$name]);
                $backends = (int) $bStmt->fetchColumn();
                if ($backends > 0) {
                    continue; // live run owns this clone — do not touch
                }
                assertSafeIdent($name, 'gc clone');
                $pdo->exec('DROP DATABASE IF EXISTS ' . qid($name));
                $dropped[] = $name;
            }
            echo json_encode(['dsn' => null, 'dropped' => $dropped]), "\n";
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
