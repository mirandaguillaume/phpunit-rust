<?php

declare(strict_types=1);

namespace Proust\Provisioning;

/**
 * SQLite adapter. A database is a FILE, so cloning is a plain file copy of the
 * migrated/seeded template — the cheapest clone of all (no DDL, no admin
 * connection). The base MUST point at a FILE: `sqlite:/abs/path/app.db`
 * (`:memory:` is per-connection and cannot be shared/cloned).
 *
 * Clones live beside the template, named `<cloneName>.sqlite`. The Rust lease
 * derives clone names from the base file's last path segment sanitized to
 * `[A-Za-z0-9_]` (so `app.db` → `app_db_pr<N>_w<N>`); `templateName()` returns
 * that same sanitized stem so the gc pattern matches.
 */
final class SqliteProvisioner extends AbstractProvisioner
{
    private string $file;

    public function __construct(string $base)
    {
        parent::__construct($base);
        // PDO form is `sqlite:<path>` (or `sqlite://<path>`); strip the scheme.
        $raw = preg_replace('#^sqlite:(//)?#i', '', $base) ?? '';
        if ($raw === '' || $raw === ':memory:') {
            throw new \RuntimeException(
                "--provision-db sqlite base must be a FILE path, not in-memory: $base"
            );
        }
        $this->file = $raw;
    }

    public function templateName(): string
    {
        // Sanitized stem of the file, matching the Rust clone-name base.
        return (string) preg_replace('/[^A-Za-z0-9_]/', '_', basename($this->file));
    }

    public function cloneOne(string $cloneName): string
    {
        $this->assertSafeIdent($cloneName, 'clone_name');
        if (! is_file($this->file)) {
            throw new \RuntimeException("sqlite template file not found: {$this->file}");
        }
        $clonePath = $this->clonePath($cloneName);
        // Idempotent: remove any stale clone of this exact name first.
        if (is_file($clonePath)) {
            @unlink($clonePath);
        }
        if (! @copy($this->file, $clonePath)) {
            $err = error_get_last()['message'] ?? 'unknown error';
            throw new \RuntimeException("sqlite clone copy failed ({$this->file} -> {$clonePath}): {$err}");
        }

        return 'sqlite:' . $clonePath;
    }

    public function dropClone(string $cloneName): void
    {
        $this->assertSafeIdent($cloneName, 'clone_name');
        $p = $this->clonePath($cloneName);
        if (is_file($p)) {
            @unlink($p);
        }
    }

    public function gcSweep(): array
    {
        // Best-effort: delete stale clone files matching the clone-name shape.
        // SQLite has no portable "is this file in use" probe, so we rely on the
        // same single-run-per-base assumption Postgres documents (CI gives each
        // job its own DB); a live concurrent run against the SAME base file is
        // out of contract.
        $pat = '/' . $this->staleClonePattern() . '/';
        $dropped = [];
        foreach (glob(dirname($this->file) . '/*.sqlite') ?: [] as $path) {
            $stem = basename($path, '.sqlite');
            if (preg_match($pat, $stem) === 1) {
                $this->assertSafeIdent($stem, 'gc clone');
                if (@unlink($path)) {
                    $dropped[] = $stem;
                }
            }
        }

        return $dropped;
    }

    private function clonePath(string $cloneName): string
    {
        return dirname($this->file) . '/' . $cloneName . '.sqlite';
    }
}
