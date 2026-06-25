<?php

declare(strict_types=1);

namespace Proust\Worker;

use Proust\Provisioning\DsnUrl;

/**
 * The per-worker environment contract: the slot-keyed state the fork master
 * injects into each child. Centralizes what was sprawled inline in the fork
 * loop — the worker id, the per-slot DB clone DSN, and the framework
 * `DATABASE_URL` repoint — behind one `apply()`, and the
 * `putenv` + `$_ENV` + `$_SERVER` triple (the "<env> convention", so
 * `getenv()`/`$_ENV`/`$_SERVER` all observe a value) behind one [`setEnv`].
 *
 * Run-wide config (PROUST_EVENT_BRIDGE, PROUST_TIMING, …) and the project's own
 * `<env>`/`<server>` vars are NOT worker context — they don't vary by slot — and
 * stay where they are.
 *
 * Idempotent: a respawned / K-batch-recycled child re-enters the fork loop with
 * the same slot, so re-applying yields the same environment.
 */
final class WorkerContext
{
    public function __construct(
        private readonly int $slot,
        private readonly ?string $dbDsn,
        private readonly bool $eventBridge,
    ) {
    }

    /** Inject this worker's per-slot environment into the current (child) process. */
    public function apply(): void
    {
        // Worker token: stable per-slot identity (resource leases, ParaTest-style
        // TEST_TOKEN parity).
        self::setEnv('PROUST_WORKER_ID', (string) $this->slot);

        if ($this->dbDsn === null || $this->dbDsn === '') {
            return;
        }
        // The marker-based per-worker connection (TestExecutor::connection) reads
        // this directly.
        self::setEnv('PROUST_DB_DSN', $this->dbDsn);

        // Parallel functional tests: a framework extension (e.g.
        // DAMADoctrineTestBundle) wraps the APP's OWN connection, which reads
        // DATABASE_URL — not PROUST_DB_DSN. Repoint it at this worker's clone,
        // deriving the per-driver URL (preserving the existing query like
        // ?serverVersion=). Gated on the event bridge so the marker path is
        // untouched and DATABASE_URL is never overridden unless needed.
        if ($this->eventBridge) {
            $url = DsnUrl::frameworkUrl($this->dbDsn, getenv('DATABASE_URL') ?: null);
            if ($url !== null) {
                self::setEnv('DATABASE_URL', $url);
            }
        }
    }

    /**
     * Set an environment variable so `getenv()`, `$_ENV` and `$_SERVER` all
     * observe it — the convention the fork worker uses for every injected var.
     */
    public static function setEnv(string $key, string $value): void
    {
        putenv("$key=$value");
        $_ENV[$key] = $value;
        $_SERVER[$key] = $value;
    }
}
