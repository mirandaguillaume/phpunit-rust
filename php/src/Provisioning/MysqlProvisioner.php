<?php

declare(strict_types=1);

namespace Proust\Provisioning;

/**
 * MySQL / MariaDB adapter. MySQL has no `CREATE DATABASE … TEMPLATE`, so a clone
 * is `CREATE DATABASE` followed by a structural+data copy of every base table
 * (`CREATE TABLE … LIKE` + `INSERT … SELECT`, with FK checks disabled during the
 * copy so table order can't violate constraints). The base MUST be URL-style:
 * `mysql://user:pass@host:port/db`.
 *
 * Credentials are embedded in the returned DSN (`mysql:…;user=…;password=…`) the
 * same way the Postgres adapter does; the consumer (TestExecutor::dbHandle)
 * extracts them and passes them as PDO constructor args, because PDO MySQL — and
 * unlike pgsql — IGNORES `user=`/`password=` inside the DSN string.
 *
 * v1 copies BASE TABLES only (schema + rows). Views / triggers / stored routines
 * are not copied; the typical test template is tables + seed data.
 */
final class MysqlProvisioner extends AbstractProvisioner
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
        $pdo->exec('CREATE DATABASE ' . self::qid($cloneName));

        $tables = $this->baseTables($template);
        if ($tables !== []) {
            $pdo->exec('SET FOREIGN_KEY_CHECKS=0');
            try {
                foreach ($tables as $t) {
                    $this->assertSafeIdent($t, 'table');
                    $src = self::qid($template) . '.' . self::qid($t);
                    $dst = self::qid($cloneName) . '.' . self::qid($t);
                    $pdo->exec("CREATE TABLE $dst LIKE $src");
                    $pdo->exec("INSERT INTO $dst SELECT * FROM $src");
                }
            } finally {
                $pdo->exec('SET FOREIGN_KEY_CHECKS=1');
            }
        }

        return $this->dsnFor($cloneName);
    }

    public function dropClone(string $cloneName): void
    {
        $this->assertSafeIdent($cloneName, 'clone_name');
        $pdo = $this->pdo();
        // Kill lingering connections so DROP succeeds even if a SIGKILLed worker
        // left one open (best-effort: needs PROCESS priv; ignore failures).
        try {
            $ids = $pdo->prepare(
                'SELECT id FROM information_schema.processlist WHERE db = ?'
            );
            $ids->execute([$cloneName]);
            foreach ($ids->fetchAll(\PDO::FETCH_COLUMN) as $id) {
                try {
                    $pdo->exec('KILL ' . (int) $id);
                } catch (\Throwable) {
                    // connection may have already closed — fine
                }
            }
        } catch (\Throwable) {
            // no PROCESS privilege — skip, DROP below still tries
        }
        $pdo->exec('DROP DATABASE IF EXISTS ' . self::qid($cloneName));
    }

    public function gcSweep(): array
    {
        $pdo = $this->pdo();
        // `_` is a LIKE wildcard, so match on the full regex instead.
        $stmt = $pdo->prepare(
            'SELECT schema_name FROM information_schema.schemata '
            . 'WHERE schema_name REGEXP :pat AND schema_name <> :base'
        );
        $stmt->execute([':pat' => $this->staleClonePattern(), ':base' => $this->templateName()]);
        $dropped = [];
        foreach ($stmt->fetchAll(\PDO::FETCH_COLUMN) as $name) {
            $bStmt = $pdo->prepare('SELECT count(*) FROM information_schema.processlist WHERE db = ?');
            $bStmt->execute([$name]);
            if ((int) $bStmt->fetchColumn() > 0) {
                continue; // a live run owns this clone — do not touch
            }
            $this->assertSafeIdent($name, 'gc clone');
            $pdo->exec('DROP DATABASE IF EXISTS ' . self::qid($name));
            $dropped[] = $name;
        }

        return $dropped;
    }

    /** Base-table names of the template schema (views/temp tables excluded). */
    private function baseTables(string $schema): array
    {
        $stmt = $this->pdo()->prepare(
            'SELECT table_name FROM information_schema.tables '
            . "WHERE table_schema = ? AND table_type = 'BASE TABLE'"
        );
        $stmt->execute([$schema]);

        return $stmt->fetchAll(\PDO::FETCH_COLUMN) ?: [];
    }

    private function pdo(): \PDO
    {
        if ($this->admin !== null) {
            return $this->admin;
        }
        $p = $this->parts;
        if (! isset($p['host'])) {
            throw new \RuntimeException(
                "unparseable --provision-db base DSN (expected mysql://user:pass@host:port/db): {$this->base}"
            );
        }
        // No default schema: CREATE DATABASE + cross-db copies don't need one.
        $dsn = sprintf('mysql:host=%s;port=%d', $p['host'], $p['port'] ?? 3306);
        $user = $p['user'] ?? (getenv('MYSQL_USER') ?: 'root');
        $pass = $p['pass'] ?? (getenv('MYSQL_PASSWORD') ?: '');

        return $this->admin = new \PDO($dsn, $user, $pass, [\PDO::ATTR_ERRMODE => \PDO::ERRMODE_EXCEPTION]);
    }

    /**
     * The clone's PDO DSN to inject as PROUST_DB_DSN, with credentials embedded
     * (extracted + passed as args by the consumer — PDO MySQL ignores them in
     * the DSN). Passwords containing `;` are unsupported (the DSN is `;`-delimited).
     */
    private function dsnFor(string $clone): string
    {
        $p = $this->parts;
        $host = $p['host'] ?? '127.0.0.1';
        $port = $p['port'] ?? 3306;
        $user = $p['user'] ?? (getenv('MYSQL_USER') ?: 'root');
        $pass = $p['pass'] ?? (getenv('MYSQL_PASSWORD') ?: '');

        return sprintf(
            'mysql:host=%s;port=%d;dbname=%s;user=%s;password=%s',
            $host, $port, $clone, $user, $pass
        );
    }

    /** Quote a MySQL identifier (backticks, escape embedded backticks). */
    private static function qid(string $id): string
    {
        return '`' . str_replace('`', '``', $id) . '`';
    }
}
