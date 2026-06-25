<?php

declare(strict_types=1);

namespace Proust\Provisioning;

/**
 * Selects the DBMS adapter from the base DSN scheme. This is the ONLY place that
 * knows which databases exist; the rest of the runner talks only to the
 * {@see Provisioner} contract.
 */
final class ProvisionerFactory
{
    public static function fromBaseDsn(string $base): Provisioner
    {
        $scheme = strtolower((string) (parse_url($base, PHP_URL_SCHEME) ?: ''));

        return match ($scheme) {
            'postgres', 'postgresql', 'pgsql' => new PgProvisioner($base),
            'sqlite', 'sqlite3' => new SqliteProvisioner($base),
            // MySQL needs the per-slot credential channel (PROUST_DB_USER/PASS +
            // driver-correct DATABASE_URL) — PDO MySQL DSNs can't embed
            // credentials, unlike pgsql. Wired in the follow-up slice.
            'mysql', 'mysqli', 'mariadb' => throw new \RuntimeException(
                'MySQL provisioning is not yet wired (needs the per-slot credential channel); '
                . 'use Postgres or SQLite for now.'
            ),
            '' => throw new \RuntimeException(
                "--provision-db base DSN has no scheme: $base (expected postgres:// / sqlite: / mysql://)"
            ),
            default => throw new \RuntimeException(
                "unsupported --provision-db scheme '$scheme' (expected postgres / sqlite / mysql)"
            ),
        };
    }
}
