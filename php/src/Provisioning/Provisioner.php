<?php

declare(strict_types=1);

namespace Proust\Provisioning;

/**
 * A per-worker database provisioner: the abstraction behind `--provision-db`.
 *
 * The runner never knows which DBMS it is talking to — it asks a `Provisioner`
 * to clone the migrated/seeded template once per worker slot, hand back a DSN,
 * and tear clones down. Postgres / SQLite / MySQL are interchangeable adapters
 * selected from the base DSN scheme by {@see ProvisionerFactory}; the DBMS is an
 * implementation detail that lives entirely inside the adapter.
 *
 * Contract:
 *  - `cloneOne` is idempotent (drops any stale clone of the same name first) and
 *    HARD-FAILS (throws) on any error — provisioning must never silently degrade.
 *  - `gcSweep` is BEST-EFFORT (a sweep failure must not abort a fresh provision)
 *    and only reclaims clones with no active connection.
 *  - clone names are pre-sanitized by the Rust lease to `^[A-Za-z0-9_]+$` (<=63
 *    bytes); adapters re-assert this before interpolating into DDL / paths.
 */
interface Provisioner
{
    /** The template identity (DB name / file) clones are derived from. */
    public function templateName(): string;

    /**
     * Idempotently create a clone named `$cloneName` from the template and
     * return the connection string to inject as `PROUST_DB_DSN`.
     */
    public function cloneOne(string $cloneName): string;

    /** Drop / remove the clone named `$cloneName` (idempotent, best-effort safe). */
    public function dropClone(string $cloneName): void;

    /**
     * Best-effort: drop stale `<base>_pr<N>_w<N>` clones from prior crashed runs
     * that have NO active connection. Never touches a clone a live run owns.
     *
     * @return list<string> the clone identifiers reclaimed
     */
    public function gcSweep(): array;
}
