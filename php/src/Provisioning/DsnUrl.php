<?php

declare(strict_types=1);

namespace Proust\Provisioning;

/**
 * Derive the framework (Doctrine DBAL) `DATABASE_URL` form of a per-slot clone's
 * PDO DSN, per driver — so the DAMA/functional path (which repoints the app's
 * OWN Doctrine connection at the worker's clone) works for Postgres, MySQL and
 * SQLite, not just Postgres.
 *
 * Pure + standalone so it is unit-tested without a database; worker_fork.php
 * calls it under the event-bridge gate.
 */
final class DsnUrl
{
    /**
     * @param  string      $pdoDsn       e.g. `pgsql:host=h;port=5432;dbname=d;user=u;password=p`,
     *                                   `mysql:host=h;port=3306;dbname=d;user=u;password=p`,
     *                                   `sqlite:/abs/path/clone.sqlite`
     * @param  string|null $existingUrl  the app's current DATABASE_URL, whose query string
     *                                   (e.g. `?serverVersion=16`) is preserved for SQL drivers
     * @return string|null               the DATABASE_URL form, or null if the scheme is unknown
     *                                   (caller then leaves DATABASE_URL untouched)
     */
    public static function frameworkUrl(string $pdoDsn, ?string $existingUrl): ?string
    {
        $colon = strpos($pdoDsn, ':');
        if ($colon === false) {
            return null;
        }
        $scheme = strtolower(substr($pdoDsn, 0, $colon));
        $rest = substr($pdoDsn, $colon + 1);

        // SQLite: the DSN body IS the file path; no host/credentials/query.
        if ($scheme === 'sqlite') {
            return 'sqlite://' . $rest;
        }

        $urlScheme = match ($scheme) {
            'pgsql' => 'postgresql',
            'mysql' => 'mysql',
            default => null,
        };
        if ($urlScheme === null) {
            return null;
        }

        $kv = [];
        foreach (explode(';', $rest) as $pair) {
            $eq = strpos($pair, '=');
            if ($eq !== false) {
                $kv[substr($pair, 0, $eq)] = substr($pair, $eq + 1);
            }
        }
        if (! isset($kv['host'], $kv['dbname'])) {
            return null;
        }

        $hostport = $kv['host'] . (isset($kv['port']) && $kv['port'] !== '' ? ':' . $kv['port'] : '');
        $cred = '';
        if (isset($kv['user'])) {
            $cred = rawurlencode($kv['user']);
            if (isset($kv['password'])) {
                $cred .= ':' . rawurlencode($kv['password']);
            }
            $cred .= '@';
        }

        // Preserve the existing DATABASE_URL query (Doctrine DBAL 4 REQUIRES
        // serverVersion for Postgres; it lives in the URL query, not the PDO DSN).
        $query = '';
        if (is_string($existingUrl) && ($qp = strpos($existingUrl, '?')) !== false) {
            $query = substr($existingUrl, $qp);
        }

        return sprintf('%s://%s%s/%s%s', $urlScheme, $cred, $hostport, $kv['dbname'], $query);
    }
}
