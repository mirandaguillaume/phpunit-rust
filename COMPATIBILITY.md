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

**CLI flags:**

| Flag | Default | Purpose |
| --- | --- | --- |
| `--project <path>` | `.` | Project root (needs `vendor/autoload.php` unless `--report-shared-fixture`). |
| `--tests-dir <path>` | `tests` | Discovery root, used only when `phpunit.xml` declares no `<testsuite>`. |
| `--configuration <path>` | auto | Path to `phpunit.xml`; auto-detects `phpunit.xml` then `phpunit.xml.dist`. |
| `--bootstrap <file>` | from XML | Bootstrap required before tests; overrides the XML `<bootstrap>`. |
| `--filter <substr>` | — | Substring match against `Class::method`. |
| `--workers <n>` | CPU cores | Parallel PHP workers; default mode clamps down by suite size. `--workers 1` = sequential. |
| `--group <name>` | — | Run only these groups (comma-separated or repeated). |
| `--exclude-group <name>` | — | Skip these groups. |
| `--testsuite <name>` | all | Run only the named `<testsuite>`. |
| `--stop-on-failure` | off | Stop after the first failed/errored test. |
| `--stop-on-defect` | off | Also stop on skipped/incomplete/risky. |
| `--worker-timeout <secs>` | `600` | Inactivity watchdog; aborts a hung run. `0` disables. |
| `--list-tests` | off | Print `Class::method` lines then exit (no tests run). |
| `--report-shared-fixture` | off | Print a SharedTransactionalFixture eligibility advisory then exit (tree-sitter only; no `composer install` needed). |
| `--dirty` | off | Run only tests impacted by uncommitted git changes (changed source → dependent tests). |
| `--bake-mocks` | off | Rewrite `createMock()` into anonymous-class stubs; requires PSR-4-resolvable interfaces. |
| `--provision-db <DSN>` | — | Base DSN for per-worker DB provisioning (also via `PROUST_DB_DSN`). |
| `--skip-db` | off | Skip `needs_db` tests instead of aborting when no DB is configured. |
| `--worker-memory-limit <v>` | `512M` | `memory_limit` inside each worker fork (`256M`, `1G`, `-1`). |
| `--worker-max-batches <n>` | `20` | Recycle each worker fork after N batches; `0` = long-lived. |
| `--profile <file>` | — | Write a Chrome Trace Format JSON timing file. |
| `--coverage-format <fmt>` | — | `clover` \| `json` \| `pcov` \| `pcov-extended` (build with `--features coverage`). |
| `--coverage-out <file>` | stdout | Coverage output destination (build with `--features coverage`). |

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

### State isolation (important limitation)

Proust runs many test classes inside one long-lived worker fork to
amortise PHP startup, which changes the isolation contract versus vanilla
PHPUnit:

- **`backupGlobals` is supported (opt-in).** `#[BackupGlobals(true)]` or
  `@backupGlobals enabled` snapshots `$GLOBALS` before `setUp` and
  restores it after `tearDown`, honouring
  `#[ExcludeGlobalVariableFromBackup]`.
- **`backupStaticAttributes` / `backupStaticProperties` are NOT
  supported.** Static properties mutated by one test stay visible to
  every later test sharing the same worker fork.
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
- Generic `<listeners>` dispatch (we parse the entries but don't execute
  user listener code — affects projects using Symfony's PhpUnitTestsListener
  for `@group legacy` handling)
- `<extensions>` (PHPUnit 10+ extension API)
- Runtime coverage (PCOV/Xdebug) — static analysis only for now
- JUnit XML / TAP / TestDox reporters
- Watch mode
- Risky test detection (no assertions, unexpected output, etc.)
