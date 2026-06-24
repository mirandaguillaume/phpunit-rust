# Changelog

## Unreleased — renamed to Proust

The project was renamed from **phpunit-rust** to **Proust** (*À la recherche du
temps perdu* — the runner recovers the time slow test suites lose).

### Binary / package

- The CLI binary is now `proust` (was `phpunit-rust`); build output is
  `target/release/proust`.
- Rust crate: `proust`. PHP namespace: `Proust\` (was `PhpunitRust\`).
- CI image is `proust-ci` (was `prust-ci`).

### Environment variables (BREAKING — no backward-compatibility shim)

Every `PHPUNIT_RUST_*` environment variable was renamed to `PROUST_*`. There is
**no fallback**: the old names are no longer read. Update any wrapper scripts,
CI, or test code that sets/reads them.

| Old | New |
|---|---|
| `PHPUNIT_RUST_DB_DSN` | `PROUST_DB_DSN` |
| `PHPUNIT_RUST_WORKER_ID` | `PROUST_WORKER_ID` |
| `PHPUNIT_RUST_SLOT` | `PROUST_SLOT` |
| `PHPUNIT_RUST_TIMING` | `PROUST_TIMING` |
| `PHPUNIT_RUST_NO_ISOLATION` | `PROUST_NO_ISOLATION` |
| `PHPUNIT_RUST_TRACE_BATCHES` | `PROUST_TRACE_BATCHES` |
| `PHPUNIT_RUST_DUMP_TESTS` | `PROUST_DUMP_TESTS` |
| `PHPUNIT_RUST_DEATH_DUMPS` | `PROUST_DEATH_DUMPS` |

The internal event-bridge gate was also renamed `PRUST_EVENT_BRIDGE` →
`PROUST_EVENT_BRIDGE` (internal; no action needed).

Apps using the ParaTest-style per-worker token must now read `PROUST_WORKER_ID`,
and DB-isolation consumers (e.g. `SharedTransactionalFixture`) read
`PROUST_DB_DSN`.

### Not renamed (intentionally)

- The GitHub repository name and clone URLs (rename the repo in GitHub Settings;
  the old URL keeps redirecting).
- Hard-coded developer checkout paths (`/home/.../PHPUnit_rust/...`) in a few
  bench scripts — these point at a local directory, not the project identity.

### Performance

- `--provision-db` now provisions every per-worker database clone in a **single**
  `provision_db.php` invocation (a new batched `provision_run` action) instead of
  spawning one PHP process per step (`gc` + `build_template` + one `clone` per
  worker = N+2 spawns). The PHP boot + project-autoload + admin-connect cost is
  paid once rather than N+2 times. Measured on Symfony Demo + PostgreSQL: the
  provisioning phase drops from 529 ms to 257 ms at `--workers 4` (−272 ms) and
  from 1019 ms to 485 ms at `--workers 8` (−534 ms), with identical test
  outcomes. Per-worker DSNs and crash-cleanup semantics are unchanged.
- New opt-in `--warmup <file>` (or `PROUST_WARMUP`): a PHP file proust `require`s
  ONCE in the fork master, after `--bootstrap` and before the fork, so workers
  inherit its warm state via copy-on-write. Booting a framework kernel here
  collapses each worker's cold first-boot (≈90 ms on Symfony) to ~1 ms — it
  removes ~90 ms of boot CPU per worker. The wall-clock payoff scales with core
  pressure: ~neutral when workers ≤ cores (boots already overlap), and a clear
  win when boots serialize (measured −5 % at `--workers 4` / −12 % at
  `--workers 8` on a 2-core box, Symfony Demo). Best-effort (a warmup error
  warns and the run continues unwarmed); zero cost when unused. See
  COMPATIBILITY.md "Warmup hook" for a Symfony example and fork-safety notes.
