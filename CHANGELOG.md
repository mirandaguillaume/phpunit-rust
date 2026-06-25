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
- The default worker-count clamp now scales the cases-per-worker divisor by
  per-worker fixed cost: functional suites clamp at 1 worker per 32 cases instead
  of 16, since each such worker pays a cold kernel boot (plus a DB clone when
  provisioned). A small functional suite no longer over-forks — a 53-test Symfony
  suite picks 2 workers instead of 4, going from +5% vs vanilla to −5% (parity).
  Unit suites and explicit `--workers N` are unchanged.
- A suite is now recognised as functional automatically — not only via
  `--provision-db`, but when any selected test extends a known framework base
  class (`KernelTestCase`, `WebTestCase`, `ApiTestCase`, `PantherTestCase`;
  extend with `PROUST_FUNCTIONAL_BASE_CLASSES`). Detection is a declared marker
  (the resolved `extends` chain), never type-reference inference, and has no
  correctness effect: it applies the conservative worker clamp and prints a
  one-line `--warmup` suggestion. So a functional suite run *without*
  `--provision-db` no longer over-forks (the +5%-on-many-cores regime), and users
  who'd benefit from `--warmup` are told about it.
- New read-only `--report-hoistable-setup` advisory (tree-sitter only, no
  `composer install`): for each concrete test class, reports which `setUp`
  `$this->P = …` fixtures could be hoisted to run ONCE instead of once-per-test
  (HOIST) vs why not (REFUSE: non-deterministic RHS / per-test ambient context
  tz·now·locale / mutation by a test), with the per-class test multiplicity. A
  faithful Rust port of the Way-3 setUp-splitter's two soundness gates (context
  scope + mutation); the foundation for an eventual warm-master setUp hoist, and
  a map of where the expensive-shared-fixture shape actually exists.
- `--provision-db` is now DBMS-agnostic behind a `Provisioner` contract
  (`php/src/Provisioning/`): the adapter is chosen from the base DSN scheme and
  the database type no longer leaks into the action handlers. **SQLite** is
  supported (`sqlite:/abs/app.db` → each worker gets a file-copy clone of the
  template, the cheapest clone of all); **Postgres** is unchanged
  (`CREATE DATABASE … TEMPLATE`), extracted into its own adapter with identical
  behaviour. SQLite covers the marker-based per-worker connection
  (`PROUST_DB_DSN`); the DAMA/functional `DATABASE_URL` repointing stays
  Postgres-only for now.
- **MySQL/MariaDB** provisioning adapter: MySQL has no `CREATE DATABASE …
  TEMPLATE`, so a clone is `CREATE DATABASE` + a per-table `CREATE TABLE … LIKE`
  / `INSERT … SELECT` copy of the base tables (FK checks disabled during the
  copy so table order can't violate constraints). Because PDO MySQL — unlike
  pgsql — ignores `user=`/`password=` in the DSN, the credentials are embedded
  in the clone DSN and `TestExecutor::dbHandle` now extracts any `user=`/
  `password=` and passes them as PDO constructor args (uniform across drivers;
  Postgres accepts both forms, SQLite carries none). `pdo_mysql` is added to the
  CI image. Covers the marker-based per-worker connection (`PROUST_DB_DSN`);
  the DAMA/functional `DATABASE_URL` repointing stays Postgres-only.
