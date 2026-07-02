# Compatibility

Lists the PHPUnit features that Proust supports today, and the ones that are explicitly deferred.

### Supported

**TestCase API** (delegated to the project's PHPUnit, so all of it works):

- All `TestCase` assertions, mocks (`createMock`, `MockBuilder`), expectation
  chaining
- `expectException`, `expectExceptionMessage`, `expectExceptionCode`
- `markTestSkipped`, `markTestIncomplete`
- `setUp` / `tearDown`, `setUpBeforeClass` / `tearDownAfterClass`
- PHPUnit 10 attribute-style lifecycle hooks: `#[Before]`, `#[After]`,
  `#[BeforeClass]`, `#[AfterClass]`

**Test discovery** (tree-sitter-based, our `discovery` crate):

- `testXxx` naming, `/** @test */` PHPDoc, `#[Test]` attribute (including
  stacked with `#[Group(...)]` and other decorations)
- `#[Ticket('...')]` attribute (parsed; used by some legacy suites for metadata)
- Inheritance chains: fully-qualified extends, abstract base classes
  outside test dirs (resolved via `composer.json` autoload)
- Custom-framework base classes: any FQCN whose last segment ends in
  `TestCase` is recognised (catches `PHPStanTestCase`, Symfony's
  `KernelTestCase` / `WebTestCase`, etc.)
- Case-insensitive method-name dedup along the inheritance chain (PHP
  semantics: a subclass `testfoo` overrides a parent `testFoo`)
- Test methods provided by `use`d **traits** (including transitive
  trait-of-trait), folded into the using class with PHP precedence
  (own class > trait > parent); the trait is never run as a class on its own.
  Project traits only — a trait declared in `vendor/` is not parsed.

**Data providers — every form PHPUnit supports:**

- `#[DataProvider("methodName")]` attribute
- `@dataProvider methodName` PHPDoc
- `#[TestWith([1, 2])]` repeatable attribute
- `#[TestWithJson('[1, 2]')]` repeatable attribute
- `@testWith [1, 2]\n        [3, 4]` PHPDoc block

Row counts are enumerated by a one-shot pre-fork PHP pass; heavy providers
(≥ 15 rows) get split across workers via stride filtering
(`row_i % N == chunk_index`).

**Skip / requires — all PHPUnit 10 attribute + PHPDoc forms:**

- `#[RequiresPhp]`, `#[RequiresPhpExtension]`, `#[RequiresFunction]`,
  `#[RequiresMethod]`, `#[RequiresOperatingSystem]`,
  `#[RequiresOperatingSystemFamily]`, `#[RequiresSetting]`,
  `#[RequiresPhpunit]`
- `@requires` PHPDoc equivalents
- All checked **before** `setUpBeforeClass` so version-gated entity files
  don't crash workers with uncatchable `E_COMPILE_ERROR`

**Groups:**

- `#[Group('name')]` and `/** @group name */` (class- and method-level)
- Inherited from the parent class along the test chain
- `phpunit.xml` `<groups><exclude><group>name</group>` filters them out

**Test dependencies:**

- `#[Depends('method')]` and `@depends method`
- Topological sort + return-value injection within a class

**Process isolation:**

- `@runInSeparateProcess` / `#[RunInSeparateProcess]` (plus
  `@runTestsInSeparateProcesses` / `@runClassInSeparateProcess` and
  their attribute forms): detected at discovery; each annotated method
  runs in its own fresh fork of the warm master, recycled before the
  next method, so process-global state from one isolated test never
  reaches the next. The runner clears PHPUnit's own
  `runTestInSeparateProcess` flag, so no nested `proc_open` sub-process
  is spawned inside the worker.

**phpunit.xml:**

- `bootstrap` attribute
- `<testsuites>` (multiple suites, **per-suite** `<exclude>`: a directory
  excluded by suite A but explicitly included by suite B is still walked
  via B)
- `<php><const>` declarations
- `<php><env>` and `<php><server>` (sets `$_ENV`/`$_SERVER`; `force`
  attribute honoured for `<env>`)
- `<php><ini>` (applied before autoload/bootstrap via `ini_set()`)
- `<groups><exclude>`
- `<listeners>` parsed but **not dispatched** (see "Not yet supported")

Proust applies the `<php>` block above and then runs the configured `bootstrap`
file — it does **not** load `.env` / `.env.test` itself, and that is correct:
framework env cascades (e.g. Symfony's `Dotenv::bootEnv`) are the app
bootstrap's job, exactly as under vanilla PHPUnit. Proust only needs `APP_ENV`
(set via `<php><env>`, or the config's own `<php>`) to be in place so the
bootstrap loads the right file — which is why detecting `phpunit.dist.xml`
matters (a missed `<php>` block leaves `APP_ENV` unset and the bootstrap loads
`.env` dev instead of `.env.test`).

**CLI flags:**

| Flag | Default | Purpose |
| --- | --- | --- |
| `--project <path>` | `.` | Project root (needs `vendor/autoload.php` unless `--report-shared-fixture` / `--report-hoistable-setup`). |
| `--tests-dir <path>` | `tests` | Discovery root, used only when `phpunit.xml` declares no `<testsuite>`. |
| `--configuration <path>` | auto | Path to `phpunit.xml`; auto-detects `phpunit.xml` then `phpunit.xml.dist`. |
| `--bootstrap <file>` | from XML | Bootstrap required before tests; overrides the XML `<bootstrap>`. |
| `--warmup <file>` | — | PHP file `require`d ONCE in the fork master before workers fork (also via `PROUST_WARMUP`); workers inherit its warm state via COW. See [Warmup hook](#warmup-hook-kernel-pre-boot). |
| `--filter <substr>` | — | Substring match against `Class::method`. |
| `--workers <n>` | CPU cores | Parallel PHP workers; default clamps by suite size — 1 worker per 16 cases, or per 32 for **functional** suites (each such worker's fixed cost is higher), so small functional suites don't over-fork. A suite is functional when `--provision-db` is set OR a selected test extends a framework base class (`KernelTestCase`/`WebTestCase`/…). `--workers 1` = sequential; an explicit count is never clamped. |
| `--group <name>` | — | Run only these groups (comma-separated or repeated). |
| `--exclude-group <name>` | — | Skip these groups. |
| `--testsuite <name>` | all | Run only the named `<testsuite>`. |
| `--stop-on-failure` | off | Stop after the first failed/errored test. |
| `--stop-on-defect` | off | Also stop on skipped/incomplete/risky. |
| `--worker-timeout <secs>` | `600` | Inactivity watchdog; aborts a hung run. `0` disables. |
| `--list-tests` | off | Print `Class::method` lines then exit (no tests run). |
| `--report-shared-fixture` | off | Print a SharedTransactionalFixture eligibility advisory then exit (tree-sitter only; no `composer install` needed). |
| `--report-hoistable-setup` | off | Print a Way-3 setUp-hoist advisory (which `setUp` fixtures could run once vs why not) then exit (tree-sitter only; read-only). |
| `--dirty` | off | Run only tests impacted by uncommitted git changes (changed source → dependent tests). |
| `--bake-mocks` | off | Rewrite `createMock()` into anonymous-class stubs; requires PSR-4-resolvable interfaces. |
| `--provision-db <DSN>` | — | Base DSN for per-worker DB provisioning; the DBMS is chosen from the scheme — `postgres://…` (`CREATE DATABASE … TEMPLATE`), `mysql://…` (`CREATE DATABASE` + `CREATE TABLE … LIKE`/`INSERT … SELECT`), or `sqlite:/abs/app.db` (file copy). Each worker gets an isolated clone of the migrated/seeded template (also via `PROUST_DB_DSN`). |
| `--skip-db` | off | Skip `needs_db` tests instead of aborting when no DB is configured. |
| `--worker-memory-limit <v>` | `512M` | `memory_limit` inside each worker fork (`256M`, `1G`, `-1`). |
| `--worker-max-batches <n>` | `20` | Recycle each worker fork after N batches; `0` = long-lived. |
| `--profile <file>` | — | Write a Chrome Trace Format JSON timing file. |
| `--log-junit <file>` | — | Write a PHPUnit-compatible JUnit XML report (same flag name as PHPUnit). Consumed as-is by GitLab, GitHub Actions, and the Jenkins JUnit plugin. |
| `--testdox` | off | Print a TestDox view (human-readable "it does X" sentences, grouped by class) after the run. |
| `--log-testdox-text <file>` | — | Write the TestDox report to a file as plain text (same flag name as PHPUnit). |
| `--coverage-format <fmt>` | — | `clover` \| `json` \| `pcov` \| `pcov-extended` (build with `--features coverage`). |
| `--coverage-out <file>` | stdout | Coverage output destination (build with `--features coverage`). |

**Reporters:**

- **JUnit XML** (`--log-junit <file>`) — PHPUnit-compatible: a root
  `<testsuites>` wrapping an aggregate `<testsuite>` that nests one
  `<testsuite>` per class of `<testcase>` elements, with `<failure>` /
  `<error>` / `<skipped>` children and per-class + root counts. `classname`
  is the dotted form (`App.Foo.BarTest`) and data-set names match PHPUnit
  (`#0` for numeric rows, `"name"` for named rows) so CI tools (GitLab,
  GitHub, Jenkins) group and diff it exactly like vanilla output. Per-test
  `assertions` and `file`/`line` attributes are omitted (not tracked by the
  runner yet); the document is still valid JUnit and consumed as-is.
- **TestDox** (`--testdox` for the console, `--log-testdox-text <file>` for a
  plain-text file) — tests grouped by class, method names humanized
  (`testFooBar` → "Foo bar"), each line marked ✔/✘/↩/∅/☢ by status.

**Robustness:**

- SIGINT / SIGKILL on `proust` reliably kills the PHP master and
  every forked child via kernel `PR_SET_PDEATHSIG` + PHP signal handlers
  — no orphan workers, no zombie 100%-CPU PHP processes after a Ctrl-C
- Each forked child becomes its own process-group leader (`posix_setpgid`);
  shutdown sends `SIGKILL` to the entire process group so grandchildren
  spawned by a test (via `proc_open`, `shell_exec`, etc.) are also reaped
- `setUpBeforeClass` and `tearDownAfterClass` failures emit per-test
  error outcomes instead of swallowing every test in the class
- Cross-class data-provider dependencies resolved via a secondary
  autoloader (Rust writes the FQCN → file index, PHP registers it with
  `spl_autoload_register`); provider exceptions are isolated per-method
  rather than crashing the whole class

**Static coverage** via the sibling `analyzer` crate (mago AST + per-test
attribution; no Xdebug / PCOV needed).

### Warmup hook (kernel pre-boot)

A framework functional test pays a one-time **cold kernel boot** the first time
it boots the app (≈90 ms on Symfony: loading + compiling the compiled container
and service classes). Every later boot in the *same* process is ~0.1 ms. Vanilla
PHPUnit pays this once for the whole suite; proust forks N workers, so each
worker pays it once — N cold boots instead of one.

`--warmup <file>` (or `PROUST_WARMUP=<file>`) closes that gap. proust `require`s
the file **once in the fork master**, after `--bootstrap` and just before the
fork. Forked workers inherit its warm state (loaded classes + the shared opcache
populated by a real `require`) via copy-on-write, so each worker's first kernel
boot drops from ~90 ms to ~1 ms. A typical Symfony warmup:

```php
<?php // tests/proust_warmup.php — run with: proust --warmup tests/proust_warmup.php
// Boot then SHUT DOWN the kernel: this loads the classes (inherited by every
// forked worker) without leaving a live DB connection open for them to share.
if (class_exists(\App\Kernel::class)) {
    $k = new \App\Kernel($_SERVER['APP_ENV'] ?? 'test', (bool) ($_SERVER['APP_DEBUG'] ?? true));
    $k->boot();
    $k->shutdown();
}
```

**Automatic functional-suite detection.** proust flags a suite as *functional*
when any selected test extends a known framework base class — by default
`KernelTestCase`, `WebTestCase`, `ApiTestCase`, `PantherTestCase` (extend the
list with the comma-separated `PROUST_FUNCTIONAL_BASE_CLASSES` env var for a
custom base). Detection is a DECLARED marker (the resolved `extends` chain),
never type-reference inference, and matches the suffix so
`App\Tests\WebTestCase` and the vendor `WebTestCase` both count. It has NO
correctness effect — it only (1) applies the conservative worker clamp above
even without `--provision-db`, and (2) prints a one-line suggestion to use
`--warmup` when you haven't. A plain `PHPUnit\Framework\TestCase` (pure unit
test) is never flagged.

Notes:
- **Opt-in and best-effort.** Without the flag nothing changes. A warmup error
  warns and the run continues *unwarmed* — it is a perf optimization, never a
  correctness gate.
- **Fork-safety is the warmup's job.** Boot then shut down (above) so no live
  connection is inherited by every worker. Symfony/Doctrine connections are lazy,
  so a boot+shutdown opens none.
- **Put it in `--warmup`, not `--bootstrap`.** `--bootstrap` is also loaded by
  the provisioning/teardown helpers, which don't fork workers — warming there
  pays the boot cost in each of those processes for no benefit.
- **The wall-clock payoff scales with core pressure.** The warmup removes
  ~90 ms of boot *CPU* per worker. When workers ≤ available cores those boots
  already overlap, so the wall gain is small (the master's one-time warmup ≈ the
  parallelized saving). When workers exceed cores — typical CI runners — the
  boots serialize and the warmup is a clear wall win (measured −5 % at
  `--workers 4` and −12 % at `--workers 8` on a 2-core box, Symfony Demo).

### State isolation (important limitation)

Proust runs many test classes inside one long-lived worker fork to
amortise PHP startup, which changes the isolation contract versus vanilla
PHPUnit:

- **`backupGlobals` is supported (opt-in).** `#[BackupGlobals(true)]` or
  `@backupGlobals enabled` snapshots `$GLOBALS` before `setUp` and
  restores it after `tearDown`, honouring
  `#[ExcludeGlobalVariableFromBackup]`.
- **`backupStaticProperties` is supported (opt-in).**
  `#[BackupStaticProperties(true)]` (or the legacy
  `@backupStaticProperties` / `@backupStaticAttributes` docblock,
  honoured only for the case-sensitive value `enabled`, exactly like
  PHPUnit) snapshots static class properties before `setUp` and restores
  them after `tearDown`, honouring `#[ExcludeStaticPropertyFromBackup]`.
  Method-level wins over class-level. Delegated to the same
  `sebastian/global-state` library PHPUnit uses, so capture/restore
  semantics match. The snapshot excludes the `PHPUnit\`,
  `SebastianBergmann\…` and **`Proust\`** namespaces — the last so the
  long-lived worker never rolls back proust's own runtime statics
  mid-batch (vanilla, running one test per process, needs no such
  guard). Opt-in only, so suites that don't request it pay nothing.
  Isolated tests (`@runInSeparateProcess`) skip the snapshot, matching
  vanilla's `inIsolation` early-return. Not yet modelled: the XML-root
  `backupStaticProperties="true"` default and the legacy
  `protected $backupStaticProperties` property form.
- Classes that touch process-global APIs (stream wrappers, error /
  exception handlers, `ini_set`, `putenv`, `setlocale`, autoload
  registration, …) are auto-detected and forced into a fresh fork per
  batch. This detection is **syntactic** and has blind spots: state
  mutated through a trait method, a free function/helper, a
  static-property accumulator, or a DI / global registry is not seen.
- **Escape hatches** when isolation bites: run with `--workers 1`
  (sequential), annotate the class with `@runInSeparateProcess`, or set
  `PROUST_NO_ISOLATION=1` only to *diagnose* pollution (it disables
  the per-batch fresh-fork). There is no per-class force-isolate flag
  beyond `@runInSeparateProcess`.
- **Database isolation** (when `--provision-db` / `PROUST_DB_DSN`
  is set) is a runner-managed PDO transaction opened before `setUp` and
  rolled back after `tearDown`. Because the runner bypasses PHPUnit's
  `runBare`, framework traits like `RefreshDatabase` /
  `DatabaseTransactions` never fire; only code routed through the
  runner's connection is rolled back, and a test that commits or runs
  DDL emits a loud `DB isolation LEAK` breadcrumb.
- **Parallel functional tests (framework DB extensions).** When the
  project configures a PHPUnit `<extensions>` bootstrap that isolates the
  app's OWN connection per test — e.g. DAMADoctrineTestBundle wrapping
  Doctrine in a transaction — Proust drives it end-to-end: the event
  bridge dispatches the extension (it arms on `TestRunner\Started` and
  begins/rolls back on `Test\PreparationStarted`), and `--provision-db`
  gives each worker its own database clone with the app's `DATABASE_URL`
  repointed at that clone (preserving the URL query, so `serverVersion`
  survives). The suite then runs in parallel at parity — on the Symfony
  Demo functional suite, `--workers 4` matched vanilla exactly with zero
  cross-worker contention. Requirements: **PostgreSQL** (the clone
  primitive is `CREATE DATABASE … TEMPLATE`; the PHP runtime needs
  `pdo_pgsql`); the app's `DATABASE_URL` must carry the platform version
  Doctrine DBAL requires (`?serverVersion=…`); and the app must NOT also
  apply its own per-worker dbname suffix (e.g. Symfony's
  `dbname_suffix: '_test%env(TEST_TOKEN)%'`) — disable it so Proust's
  per-worker clone DSN is authoritative.
- **Provisioning is DBMS-agnostic behind a `Provisioner` contract** (the
  adapter is chosen from the base DSN scheme). The marker-based per-worker
  connection (`TestExecutor::connection()` / `PROUST_DB_DSN`) supports
  **Postgres** (`CREATE DATABASE … TEMPLATE`), **MySQL/MariaDB** (`CREATE
  DATABASE` + per-table `CREATE TABLE … LIKE` / `INSERT … SELECT`, base tables
  only, FK checks off during the copy), and **SQLite** (file copy of the
  template). Credentials for MySQL are embedded in the clone DSN and extracted
  as PDO constructor args by `dbHandle` — PDO MySQL, unlike pgsql, ignores
  `user=`/`password=` in the DSN string. The DAMA/functional path above —
  repointing the app's `DATABASE_URL` at the clone — derives the URL **per
  driver** (`DsnUrl`): `postgresql://` / `mysql://` (host:port/db with
  credentials, preserving the existing query such as `?serverVersion=`) and
  `sqlite:///path`. The MySQL runtime needs `pdo_mysql` (in the CI image). The
  Postgres functional path is CI-gated; MySQL/SQLite share the same derivation
  but aren't yet exercised by a dedicated CI job.

#### Consuming the per-worker clone (framework-agnostic)

Proust provisions the clone and exposes its DSN as `PROUST_DB_DSN` — *how* a
suite uses it is the app's choice. The framework is the **app's** detail, not
Proust's; there is no framework adapter to configure. Three ways, in order of
"no app code needed":

1. **Marker traits (zero app code).** A test using `RefreshDatabase` /
   `DatabaseTransactions` (or `SharedTransactionalFixture`) gets the runner's own
   connection via `TestExecutor::connection()` — already pointed at this worker's
   clone, credentials handled, with per-test transaction reset. Works for any
   framework or none.
2. **Doctrine / Symfony (automatic).** With `--provision-db` + a DAMA-style
   `<extensions>` bootstrap, Proust repoints the app's `DATABASE_URL` at the
   clone (see above) — the app's own Doctrine connection follows, no code.
3. **Any other framework / plain app (read it yourself).** Read `PROUST_DB_DSN`
   in your own config and point your connection at it — Proust never assumes
   your framework's config convention. For example, Laravel `config/database.php`:

   ```php
   // PROUST_DB_DSN is a PDO DSN: "<driver>:host=…;port=…;dbname=…;user=…;password=…"
   // (or "sqlite:/abs/path"). Feed it into Laravel's connection config.
   if ($dsn = getenv('PROUST_DB_DSN')) {
       parse_str(strtr(substr($dsn, strpos($dsn, ':') + 1), ';', '&'), $p);
       $connections['mysql'] = array_merge($connections['mysql'], [
           'host' => $p['host'] ?? '127.0.0.1', 'port' => $p['port'] ?? '3306',
           'database' => $p['dbname'] ?? '', 'username' => $p['user'] ?? '',
           'password' => $p['password'] ?? '',
       ]);
   }
   ```

### Not yet supported (deferred)

- **`.phpt` tests.** PHPUnit's file-based test format (a mini-INI with
  `--TEST--` / `--FILE--` / `--EXPECT--` sections) is not a PHP class, and
  Proust discovers tests by reflecting `*Test.php` **classes** — so
  `.phpt` files are invisible to it by construction. This is a structural gap,
  not a bug: supporting it means teaching the runner a second execution model.
  It mainly affects PHPUnit's own suite (`sebastianbergmann/phpunit`), whose
  `end-to-end` testsuite is ~1000 `.phpt` files. For that reason the benchmark
  harness runs vanilla phpunit-itself with `--testsuite unit` (the `.phpt`-free
  suite) so the comparison is like-for-like; see `bench/bench_host.sh`.
- Generic `<listeners>` dispatch — the **legacy** (pre-10) mechanism: we parse
  the entries but don't execute user listener code (affects projects using
  Symfony's PhpUnitTestsListener for `@group legacy` handling). The modern
  PHPUnit 10+ `<extensions>` API, by contrast, **is** bootstrapped (opt-in via
  the event bridge, best-effort across PHPUnit versions) — see "State isolation"
  above; it is how DAMADoctrineTestBundle runs under Proust.
- Runtime coverage (PCOV/Xdebug) — static analysis only for now
- TAP reporter (JUnit XML and TestDox **are** supported — see "Reporters" above)
- Watch mode
- Risky test detection (no assertions, unexpected output, etc.)
