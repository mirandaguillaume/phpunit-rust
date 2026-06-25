<?php

declare(strict_types=1);

namespace Proust\Provisioning;

/**
 * PostgreSQL adapter. Cloning uses `CREATE DATABASE … TEMPLATE`, the cheap
 * storage-level copy primitive — the original (and fastest) provisioning path.
 * Admin DDL runs against the `postgres` maintenance DB (CREATE/DROP DATABASE
 * cannot run inside a transaction nor against the target DB itself). The base
 * MUST be URL-style: `postgres://user:pass@host:port/db`.
 */
final class PgProvisioner extends AbstractProvisioner
{
    private ?\PDO $admin = null;

    public function templateName(): string
    {
        $path = parse_url($this->base, PHP_URL_PATH) ?: '';

        return ltrim($path, '/') ?: 'app';
    }

    public function cloneOne(string $cloneName): string
    {
        $this->assertSafeIdent($cloneName, 'clone_name');
        $template = $this->templateName();
        $this->assertSafeIdent($template, 'template');
        $pdo = $this->pdo();
        // Idempotent: drop any stale clone of this exact name from a prior crash.
        $pdo->exec('DROP DATABASE IF EXISTS ' . self::qid($cloneName));
        $pdo->exec('CREATE DATABASE ' . self::qid($cloneName) . ' TEMPLATE ' . self::qid($template));

        return $this->dsnFor($cloneName);
    }

    public function dropClone(string $cloneName): void
    {
        $this->assertSafeIdent($cloneName, 'clone_name');
        $pdo = $this->pdo();
        // Terminate lingering backends so DROP succeeds even if a SIGKILLed
        // worker left a connection open, then DROP IF EXISTS.
        $stmt = $pdo->prepare(
            'SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = ?'
        );
        $stmt->execute([$cloneName]);
        $pdo->exec('DROP DATABASE IF EXISTS ' . self::qid($cloneName));
    }

    public function gcSweep(): array
    {
        $pdo = $this->pdo();
        $stmt = $pdo->prepare(
            'SELECT datname FROM pg_database WHERE datname ~ :pat AND datname <> :base'
        );
        $stmt->execute([':pat' => $this->staleClonePattern(), ':base' => $this->templateName()]);
        $dropped = [];
        foreach ($stmt->fetchAll(\PDO::FETCH_COLUMN) as $name) {
            $bStmt = $pdo->prepare('SELECT count(*) FROM pg_stat_activity WHERE datname = ?');
            $bStmt->execute([$name]);
            if ((int) $bStmt->fetchColumn() > 0) {
                continue; // live run owns this clone — do not touch
            }
            $this->assertSafeIdent($name, 'gc clone');
            $pdo->exec('DROP DATABASE IF EXISTS ' . self::qid($name));
            $dropped[] = $name;
        }

        return $dropped;
    }

    /** Lazily connect to the `postgres` maintenance DB for CREATE/DROP DATABASE. */
    private function pdo(): \PDO
    {
        if ($this->admin !== null) {
            return $this->admin;
        }
        $p = $this->parts;
        if (! isset($p['host'])) {
            throw new \RuntimeException(
                "unparseable --provision-db base DSN (expected postgres://user:pass@host:port/db): {$this->base}"
            );
        }
        $dsn = sprintf('pgsql:host=%s;port=%d;dbname=postgres', $p['host'], $p['port'] ?? 5432);
        $user = $p['user'] ?? (getenv('PGUSER') ?: 'postgres');
        $pass = $p['pass'] ?? (getenv('PGPASSWORD') ?: '');

        return $this->admin = new \PDO($dsn, $user, $pass, [\PDO::ATTR_ERRMODE => \PDO::ERRMODE_EXCEPTION]);
    }

    /**
     * The clone's PDO DSN to inject as PROUST_DB_DSN. It MUST embed credentials
     * (pgsql DSNs accept `user=`/`password=`), because the consumer does
     * `new \PDO($dsn)` with no separate user/pass args.
     */
    private function dsnFor(string $clone): string
    {
        $p = $this->parts;
        $host = $p['host'] ?? '127.0.0.1';
        $port = $p['port'] ?? 5432;
        $user = $p['user'] ?? (getenv('PGUSER') ?: 'postgres');
        $pass = $p['pass'] ?? (getenv('PGPASSWORD') ?: '');

        return sprintf(
            'pgsql:host=%s;port=%d;dbname=%s;user=%s;password=%s',
            $host, $port, $clone, $user, $pass
        );
    }

    /** Quote a Postgres identifier (double-quote, escape embedded quotes). */
    private static function qid(string $id): string
    {
        return '"' . str_replace('"', '""', $id) . '"';
    }
}
