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
