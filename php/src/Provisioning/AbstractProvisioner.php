<?php

declare(strict_types=1);

namespace Proust\Provisioning;

/**
 * Shared base for DBMS adapters: base-DSN parsing, identifier safety, and the
 * stale-clone name pattern. Concrete adapters implement the DBMS-specific clone
 * mechanism (Postgres `CREATE DATABASE … TEMPLATE`, SQLite file copy, MySQL
 * schema/data copy) on top of these.
 */
abstract class AbstractProvisioner implements Provisioner
{
    /** @var array<string, mixed> parsed components of the base DSN */
    protected array $parts;

    public function __construct(protected string $base)
    {
        $parts = parse_url($base);
        if ($parts === false) {
            throw new \RuntimeException("unparseable --provision-db base DSN: $base");
        }
        $this->parts = $parts;
    }

    /**
     * Defense-in-depth: every identifier interpolated into DDL or a filesystem
     * path must already be sanitized by the Rust lease (`^[A-Za-z0-9_]+$`, <=63
     * bytes). Reject anything else hard, BEFORE use, so a malformed request can
     * never reach the database or escape the clone directory.
     */
    final protected function assertSafeIdent(string $id, string $what): void
    {
        if ($id === '' || strlen($id) > 63 || ! preg_match('/^[A-Za-z0-9_]+$/', $id)) {
            throw new \RuntimeException(
                "unsafe $what identifier (expected ^[A-Za-z0-9_]+\$, <=63 bytes): $id"
            );
        }
    }

    /**
     * POSIX-ERE matching the clone shape `<templateName>_pr<digits>_w<digits>`
     * (excluding the template itself), for the gc sweep. The base name's regex
     * metacharacters are escaped.
     */
    final protected function staleClonePattern(): string
    {
        $base = $this->templateName();
        $re = preg_replace('/[.^$*+?()\\[\\]{}|\\\\]/', '\\\\$0', $base);

        return '^' . $re . '_pr[0-9]+_w[0-9]+$';
    }
}
