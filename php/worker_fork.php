<?php

declare(strict_types=1);

/**
 * Fork-based batch worker master.
 *
 * CLI usage:
 *   php worker_fork.php \
 *     --autoload   /path/vendor/autoload.php \
 *     [--bootstrap /path/bootstrap.php]      \
 *     [--defines   '[[\"K\",\"V\"]]']        \
 *     --child-stdin-fds  7,9,11,13           \
 *     --child-stdout-fds 8,10,12,14
 *
 * Rust streams newline-delimited BatchPlan JSONs per child-stdin-fd
 * (work-stealing pool); children stream TestOutcome JSON lines back
 * and emit {"batch_done": true} after each batch as a ready signal.
 * Children exit when Rust closes their stdin (EOF).
 *
 * Requires: pcntl extension (standard PHP CLI on Linux/macOS).
 */

error_reporting(E_ALL & ~E_DEPRECATED);
@set_time_limit(0);

// Reserved child→master exit codes that signal a VOLUNTARY child exit. The
// master's SIGCHLD handler treats ONLY these as intentional; every other exit
// status — crucially a bare exit(0)/die() from test code mid-batch — is a
// crash and gets a `slot_died` notice so Rust can recover the lost batch.
//
// Why a reserved code rather than exit(0): a child that finishes its K-batch
// quota or a force_exit_after batch exits voluntarily so the master can fork a
// warm replacement. That used to be exit(0) — indistinguishable from a test or
// teardown calling exit(0)/die() in the middle of a batch. The implicit signal
// stalled the run: the master forked a replacement (which blocks on stdin) and
// emitted no slot_died, so the dispatcher waited until the 600 s watchdog
// mass-errored everything. Making voluntariness EXPLICIT removes the ambiguity.
//
// 6 is chosen because PHP fatals/segfaults do not surface as exit 6 (fatals →
// 255, signals → 128+signo), so it can't collide with a real crash. 7 keeps
// its existing meaning: the child saw EOF on stdin (Rust closed the slot).
const WORKER_EXIT_VOLUNTARY_RECYCLE = 6; // K-batch / force_exit_after recycle
const WORKER_EXIT_STDIN_EOF         = 7; // Rust closed our stdin; slot is done

// Main-body guard. The top-level `function` declarations below (write_line,
// proust_rmtree, runChild, emitError) are hoisted at compile time and so
// remain callable even though we `return` here — which lets the PHP unit tests
// `require` this file purely to exercise those helpers WITHOUT spawning the
// fork-pool master. The master only runs when this file is the entry script
// (php worker_fork.php …). We key on $_SERVER['SCRIPT_FILENAME'] — the actual
// entry script PHP is executing — because $argv/$argc are NOT populated inside
// an included file's scope (they exist only in the top-level script scope), so
// an argv-based check misfires under PHPUnit. When this file is included by a
// test, SCRIPT_FILENAME is the phpunit binary (or empty for `php -r`), never
// this file, so we bail before any side effect (arg parsing, vendor autoload,
// fork loop).
$__entryScript = $_SERVER['SCRIPT_FILENAME'] ?? ($_SERVER['argv'][0] ?? '');
if ($__entryScript === '' || realpath($__entryScript) !== realpath(__FILE__)) {
    return;
}

// POC instrumentation: write phase timings to STDERR. Turn on with
// PROUST_TIMING=1 in the env. Output format:
//   [TIMING] phase=name delta_ms=X total_ms=Y
$__t0 = microtime(true);
$__tprev = $__t0;
$__timing_enabled = getenv('PROUST_TIMING') === '1';
$__log_phase = function(string $name) use (&$__tprev, $__t0, $__timing_enabled): void {
    if (!$__timing_enabled) return;
    $now = microtime(true);
    fwrite(STDERR, sprintf("[TIMING] phase=%-22s delta_ms=%6.1f total_ms=%6.1f\n",
        $name, ($now - $__tprev) * 1000, ($now - $__t0) * 1000));
    $__tprev = $now;
};
$__log_phase('start');

require_once __DIR__ . '/vendor/autoload.php';
$__log_phase('worker_vendor_autoload');

use Proust\OutcomeBuilder;
use Proust\TestExecutor;

/**
 * Write one worker payload to $stream as a newline-delimited JSON line.
 *
 * Centralizes every worker→Rust write so the JSON encoding goes through a
 * single hardened path. The historical pattern `fwrite($stream,
 * json_encode($x) . "\n")` silently lost a line whenever json_encode()
 * returned false (invalid UTF-8 in an exception message / stack trace), which
 * is a test-count parity violation. OutcomeBuilder::encodeLine() guarantees a
 * non-empty, valid-JSON line is always produced (with a shape-preserving
 * fallback when the payload is unencodable), so a TestOutcome is never dropped.
 *
 * @param resource             $stream
 * @param array<string, mixed> $payload
 */
function write_line($stream, array $payload): void
{
    fwrite($stream, OutcomeBuilder::encodeLine($payload));
    fflush($stream);
}

/**
 * Recursively remove a filesystem path with lstat (do-not-follow-symlinks)
 * semantics. Used by the per-child TMPDIR shutdown cleanup.
 *
 * CRITICAL parity/robustness note: this MUST NOT follow symlinks. The earlier
 * implementation decided recursion with is_dir($sub), which dereferences a
 * symlink to its target — so a test that writes a symlink into its TMPDIR
 * pointing at a directory OUTSIDE the worker temp (PHPUnit's own end-to-end
 * fixtures legitimately create such links) would have the *link target's*
 * contents recursively deleted at every worker exit. That is silent data loss
 * outside the sandbox. lstat semantics fix it: a symlink is @unlink'd as a
 * link (never traversed), a real directory is recursed into, anything else is
 * @unlink'd. The TOP path is guarded the same way so a tree whose root is
 * itself a symlink unlinks the link rather than nuking the target.
 *
 * Best-effort: every removal is silenced; a child exiting can't afford to
 * fatal on a cleanup race, and tmpfs reclaims the rest when the container ends.
 */
function proust_rmtree(string $path): void
{
    // Guard the top path with lstat semantics: a symlink (even one whose
    // target is a directory) is unlinked as a link, never traversed.
    if (is_link($path)) {
        @unlink($path);
        return;
    }
    if (!is_dir($path)) {
        @unlink($path);
        return;
    }
    foreach (@scandir($path) ?: [] as $entry) {
        if ($entry === '.' || $entry === '..') {
            continue;
        }
        $sub = $path . '/' . $entry;
        if (is_link($sub)) {
            // Symlink: remove the link itself, do NOT follow it. This is the
            // load-bearing branch — is_dir($sub) would have followed the link.
            @unlink($sub);
        } elseif (is_dir($sub)) {
            proust_rmtree($sub);
        } else {
            @unlink($sub);
        }
    }
    @rmdir($path);
}

// ---------------------------------------------------------------------------
// 1. Parse CLI args
// ---------------------------------------------------------------------------
$args = [];
for ($i = 1; $i < $argc; $i++) {
    if (!str_starts_with($argv[$i], '--')) {
        continue;
    }
    $key = substr($argv[$i], 2);
    // Valueless boolean flags consume no following value.
    if ($key === 'inline') {
        $args[$key] = '1';
        continue;
    }
    if (isset($argv[$i + 1])) {
        $args[$key] = $argv[++$i];
    }
}

$autoload        = $args['autoload']           ?? null;
$bootstrap       = $args['bootstrap']          ?? null;
$warmup          = $args['warmup']             ?? null;
$definesJson     = $args['defines']            ?? '[]';
$envJson         = $args['env']                ?? '[]';
$serverJson      = $args['server']             ?? '[]';
$iniJson         = $args['ini']                ?? '[]';
$varsJson        = $args['vars']               ?? '[]';
$childStdinFdsStr  = $args['child-stdin-fds']    ?? '';
$childStdoutFdsStr = $args['child-stdout-fds']   ?? '';
$classMapFile      = $args['class-map-file']     ?? null;
$workerMemoryLimit = $args['worker-memory-limit'] ?? '512M';
$maxBatches        = (int) ($args['max-batches-per-child'] ?? '0'); // 0 = unlimited
$perSlotDsnJson    = $args['per-slot-dsn']        ?? '[]';
// L3: single-process (no-fork) mode — run the per-batch loop in THIS master process. The runner
// only sets it for exactly one dispatch-safe slot, so there is no global-state bleed risk and no
// child to recover; it matches vanilla's single-process model.
$inline            = !empty($args['inline']);

if ($autoload === null || $childStdinFdsStr === '' || $childStdoutFdsStr === '') {
    fwrite(STDERR, "worker_fork.php: missing --autoload, --child-stdin-fds, --child-stdout-fds\n");
    exit(1);
}

$defines       = json_decode($definesJson,    true) ?? [];
$envVars       = json_decode($envJson,        true) ?? [];
$serverVars    = json_decode($serverJson,     true) ?? [];
$iniVars       = json_decode($iniJson,        true) ?? [];
$varVars       = json_decode($varsJson,       true) ?? [];
$perSlotDsn    = json_decode($perSlotDsnJson, true) ?? [];
$classMapExtra = ($classMapFile !== null && is_file($classMapFile))
    ? (json_decode(file_get_contents($classMapFile), true) ?? [])
    : [];

// Apply <ini> first so error_reporting / memory_limit etc. are in effect
// before we run any user code in autoload/bootstrap.
foreach ($iniVars as $pair) {
    if (is_array($pair) && count($pair) === 2 && is_string($pair[0])) {
        @ini_set($pair[0], (string) $pair[1]);
    }
}
// Apply <env>: each is [name, value, force]. PHPUnit's force=false means
// "don't clobber a value already in the shell environment".
foreach ($envVars as $entry) {
    if (!is_array($entry) || count($entry) !== 3) continue;
    [$name, $value, $force] = $entry;
    if (!is_string($name)) continue;
    if (!$force && getenv($name) !== false) continue;
    putenv("$name=$value");
    $_ENV[$name] = $value;
}
// Apply <server>: populate $_SERVER (commonly used for HTTPS, SCRIPT_NAME,
// etc.). PHPUnit does NOT honour `force` on <server> in any version
// I can find, so we always set.
foreach ($serverVars as $pair) {
    if (is_array($pair) && count($pair) === 2 && is_string($pair[0])) {
        $_SERVER[$pair[0]] = $pair[1];
    }
}
// Apply <var>: PHPUnit's PhpHandler assigns each to $GLOBALS[$name] (NOT the
// environment). We set it in the master so every forked child inherits it via
// COW. doctrine-orm reads $GLOBALS['db_driver'] / 'db_memory' to find its test DB.
foreach ($varVars as $pair) {
    if (is_array($pair) && count($pair) === 2 && is_string($pair[0])) {
        $GLOBALS[$pair[0]] = $pair[1];
    }
}
$childStdinFds  = array_map('intval', explode(',', $childStdinFdsStr));
$childStdoutFds = array_map('intval', explode(',', $childStdoutFdsStr));
$n              = count($childStdinFds);

if ($n !== count($childStdoutFds) || $n < 1) {
    fwrite(STDERR, "worker_fork.php: stdin/stdout fd count mismatch or zero\n");
    exit(1);
}

// ---------------------------------------------------------------------------
// 2. Layer 1: autoloader
// ---------------------------------------------------------------------------
ob_start();
try {
    require_once $autoload;
    // Walk registered autoloaders and disable classmap-authoritative mode
    // on any Composer ClassLoader. Projects optimised for shipping (Composer
    // itself, some PSR-4 .phars) enable this mode by default — it kills
    // PSR-4 lookups for any class not in the pre-built classmap. Test
    // classes live under `tests/` which is excluded from the production
    // classmap, so they would be unloadable. Disabling the flag re-enables
    // PSR-4 fallback without affecting normal classmap hits.
    foreach (spl_autoload_functions() ?: [] as $fn) {
        if (is_array($fn) && isset($fn[0]) && is_object($fn[0])
            && $fn[0] instanceof \Composer\Autoload\ClassLoader
            && method_exists($fn[0], 'isClassMapAuthoritative')
            && $fn[0]->isClassMapAuthoritative()) {
            $fn[0]->setClassMapAuthoritative(false);
        }
    }
    // Register a secondary autoloader for test classes that Composer's
    // classmap doesn't cover (e.g. test helpers whose providers call sibling
    // test classes). The map is built from the runner's discovery index.
    if (!empty($classMapExtra)) {
        spl_autoload_register(static function (string $class) use ($classMapExtra): void {
            if (isset($classMapExtra[$class]) && is_file($classMapExtra[$class])) {
                require_once $classMapExtra[$class];
            }
        });
    }
    foreach ($defines as $pair) {
        if (is_array($pair) && count($pair) === 2
            && is_string($pair[0]) && !defined($pair[0])) {
            define($pair[0], $pair[1]);
        }
    }
    // PHPUnit 10+ Configuration registry initialisation.
    if (class_exists(\PHPUnit\TextUI\Configuration\Registry::class)) {
        try {
            \PHPUnit\TextUI\Configuration\Registry::init(
                (new \PHPUnit\TextUI\CliArguments\Builder)->fromParameters([]),
                \PHPUnit\TextUI\XmlConfiguration\DefaultConfiguration::create(),
            );
        } catch (\Throwable) { /* version drift; non-fatal */ }
    }
} catch (\Throwable $e) {
    ob_end_clean();
    fwrite(STDERR, "worker_fork.php: autoload failed: " . $e->getMessage() . "\n");
    exit(1);
}
ob_end_clean();
$__log_phase('project_autoload');

// ---------------------------------------------------------------------------
// 3. Layer 2: bootstrap (optional)
// ---------------------------------------------------------------------------
if ($bootstrap !== null && is_file($bootstrap)) {
    ob_start();
    try {
        require_once $bootstrap;
    } catch (\Throwable $e) {
        ob_end_clean();
        fwrite(STDERR, "worker_fork.php: bootstrap failed: " . $e->getMessage() . "\n");
        exit(1);
    }
    ob_end_clean();
}
$__log_phase('bootstrap');

// ---------------------------------------------------------------------------
// 3b. Event bridge (option A): bootstrap the configured PHPUnit <extensions>
// and seal the Event\Facade, so the per-test lifecycle events emitted by
// TestExecutor reach event subscribers (e.g. DAMADoctrineTestBundle's per-test
// DB transaction wrapping). Done in the master so all forked children inherit
// the sealed facade + registered subscribers via COW; the subscribers only
// open DB connections lazily per-test inside each child, so nothing is shared.
// PROUST_EVENT_BRIDGE is exported ONLY when >=1 extension is present, so suites
// without <extensions> (every OSS lib) pay zero per-test emission overhead.
// ---------------------------------------------------------------------------
$__cfgPath = null;
if (is_string($autoload)) {
    $__root = dirname($autoload, 2);
    foreach (['phpunit.xml', 'phpunit.dist.xml', 'phpunit.xml.dist'] as $__n) {
        if (is_file("$__root/$__n")) { $__cfgPath = "$__root/$__n"; break; }
    }
}
// Cheap pre-check: only attempt the (version-sensitive) full config load when
// the config actually declares <extensions>. Suites without any (every OSS lib)
// skip silently and pay nothing — and we avoid tripping over PHPUnit-version
// differences in the config loader for projects that don't need the bridge.
if ($__cfgPath !== null && !str_contains((string) @file_get_contents($__cfgPath), '<extensions')) {
    $__cfgPath = null;
}
if ($__cfgPath !== null
    && class_exists(\PHPUnit\Event\Facade::class)
    && class_exists(\PHPUnit\Runner\Extension\ExtensionBootstrapper::class)
    && class_exists(\PHPUnit\TextUI\XmlConfiguration\Loader::class)) {
    try {
        $__xml = (new \PHPUnit\TextUI\XmlConfiguration\Loader())->load($__cfgPath);
        // Cross-version: CliArguments\Builder::fromParameters() takes one array
        // on PHPUnit 11 but two on some other lines. Build the empty CLI config
        // with the arity this PHPUnit actually wants instead of assuming one.
        $__cliReq = (new \ReflectionMethod(\PHPUnit\TextUI\CliArguments\Builder::class, 'fromParameters'))
            ->getNumberOfRequiredParameters();
        $__cli = $__cliReq >= 2
            ? (new \PHPUnit\TextUI\CliArguments\Builder())->fromParameters([], [])
            : (new \PHPUnit\TextUI\CliArguments\Builder())->fromParameters([]);
        \PHPUnit\TextUI\Configuration\Registry::init($__cli, $__xml);
        $__cfg = \PHPUnit\TextUI\Configuration\Registry::get();
        $__bootstrappers = $__cfg->extensionBootstrappers();
        if (count($__bootstrappers) > 0) {
            $__bs = new \PHPUnit\Runner\Extension\ExtensionBootstrapper(
                $__cfg,
                new \PHPUnit\Runner\Extension\Facade(),
            );
            foreach ($__bootstrappers as $__b) {
                // extensionBootstrappers() yields arrays: {className, parameters}.
                $__bs->bootstrap($__b['className'], $__b['parameters']);
            }
            \PHPUnit\Event\Facade::instance()->seal();
            // Emit TestRunner\Started ONCE in the master: DAMA's subscriber sets
            // StaticDriver::setKeepStaticConnections(true) (a static), which the
            // forked children inherit via COW — that flag is what actually arms
            // the per-test transaction wrapping. Without it, the per-test
            // PreparationStarted events are no-ops.
            \PHPUnit\Event\Facade::instance()->emitter()->testRunnerStarted();
            // Emit TestRunner\Finished on process exit (inherited by every forked
            // child via COW; fires once per process). DAMA does its final
            // rollback + StaticDriver::setKeepStaticConnections(false) here. A
            // SIGKILLed child skips this, but the DB connection-close rollback
            // still isolates its last test, so correctness holds either way.
            register_shutdown_function(static function (): void {
                try {
                    \PHPUnit\Event\Facade::instance()->emitter()->testRunnerFinished();
                } catch (\Throwable) {
                    // shutdown is best-effort; nothing actionable here.
                }
            });
            putenv('PROUST_EVENT_BRIDGE=1');
            $_ENV['PROUST_EVENT_BRIDGE'] = '1';
            fwrite(STDERR, 'worker_fork.php: event bridge active (' . count($__bootstrappers) . " extension(s))\n");
        }
    } catch (\Throwable $__e) {
        fwrite(STDERR, 'worker_fork.php: event bridge skipped: ' . $__e->getMessage() . "\n");
    }
}
$__log_phase('event_bridge');

// ---------------------------------------------------------------------------
// 3c. Master-only warmup (--warmup). Run the app-provided warmup file ONCE
//     here — after autoload+bootstrap, BEFORE the fork — so every forked child
//     inherits its warm state (loaded classes + the shared opcache populated by
//     a real include) via copy-on-write. Booting a framework kernel here
//     collapses each worker's cold first-boot (~90ms on Symfony) to ~1ms, a win
//     that grows with worker count. This is fundamentally different from the
//     removed opcache_compile_file pre-warm (section 4): a REAL include defines
//     classes in process memory (COW-inherited) and never early-binds ghost
//     classes, so it carries none of that mechanism's redeclare hazard.
//
//     Best-effort: a warmup error warns and the run continues UNWARMED — this is
//     a perf optimization, never a correctness gate. The app's warmup owns
//     fork-safety: boot then SHUT DOWN the kernel so no live DB connection is
//     left open for children to share. Run in an isolated scope so the warmup's
//     locals never leak into the master (and thus into every child).
// ---------------------------------------------------------------------------
if ($warmup !== null) {
    if (is_file($warmup)) {
        try {
            (static function (string $__warmupFile): void { require $__warmupFile; })($warmup);
            fwrite(STDERR, "worker_fork.php: warmup ran ($warmup)\n");
        } catch (\Throwable $__e) {
            fwrite(STDERR, 'worker_fork.php: warmup failed (continuing unwarmed): ' . $__e->getMessage() . "\n");
        }
    } else {
        fwrite(STDERR, "worker_fork.php: warmup file not found (continuing unwarmed): $warmup\n");
    }
}
$__log_phase('warmup');

// ---------------------------------------------------------------------------
// 4. NO opcache pre-warm — deliberately. The master used to
//    opcache_compile_file() every classmap file here so children would
//    inherit compiled opcodes. Root-caused and removed after it was proven
//    to be the source of the CI-only parity-gate worker deaths:
//
//    * opcache_compile_file early-binds any class whose parent is already
//      resolvable at compile time, DECLARING it in the master with no
//      include-registry entry. Every fork inherits the ghost class; the
//      first child that include_once's its file re-executes the file and
//      dies on "Cannot redeclare class" (exit 255), which the orchestrator
//      books as a worker death — the exact carbon/doctrine/php-parser
//      parity drift. The leaked SET varies with master state and file
//      order (doctrine leaked 292 classes on CI), which is why it never
//      reproduced locally. Static guards (multi-class files, top-level
//      functions) were holed three times; the mechanism is inherently
//      state-dependent and cannot be guarded statically.
//    * The benefit was measured to be zero on PHP 8.4 CLI: compiled
//      opcodes do NOT reach forked children (100 prewarmed vs 100 cold
//      files: 18.2ms vs 17.2ms child include time, speedup 0.94x).
//
//    All cost, no benefit: do not re-add. The death-row diagnostics in
//    runChild() keep reporting prewarm/leak breadcrumbs (now 'off') so a
//    regression would be visible in the parity gate's forensics.
// ---------------------------------------------------------------------------
$__log_phase('opcache_prewarm');

// ---------------------------------------------------------------------------
// 6. Open PHP stream handles for all child FDs BEFORE fork so children inherit
// ---------------------------------------------------------------------------
$childStdinStreams  = [];
$childStdoutStreams = [];
for ($i = 0; $i < $n; $i++) {
    $childStdinStreams[$i]  = fopen('php://fd/' . $childStdinFds[$i],  'r');
    $childStdoutStreams[$i] = fopen('php://fd/' . $childStdoutFds[$i], 'w');
    if ($childStdinStreams[$i] === false || $childStdoutStreams[$i] === false) {
        fwrite(STDERR, "worker_fork.php: failed to open fd for slot $i\n");
        exit(1);
    }
}

// ---------------------------------------------------------------------------
// 7. Install master-side signal handlers
// ---------------------------------------------------------------------------
// Triggered by:
//   - Rust's Drop sending SIGTERM during normal shutdown
//   - The kernel's PR_SET_PDEATHSIG firing SIGTERM when Rust dies of any
//     other cause (SIGKILL, panic before Drop, OOM, …)
//   - User hitting Ctrl-C on proust (SIGINT propagates to the
//     process group)
// We SIGKILL every forked child immediately so a child stuck in setUp or
// an infinite-loop test can't outlive its parent.
$childPids = [];
pcntl_async_signals(true);
$signalHandler = function (int $sig) use (&$childPids): void {
    foreach ($childPids as $pid) {
        @posix_kill(-$pid, SIGKILL);  // negative PID = kill entire process group
    }
    exit(128 + $sig);
};
pcntl_signal(SIGTERM, $signalHandler);
pcntl_signal(SIGINT,  $signalHandler);
pcntl_signal(SIGHUP,  $signalHandler);

// ---------------------------------------------------------------------------
// 8. Fork N children. When $maxBatches > 0 we behave as a fork-server: a
//    child exits voluntarily after processing $maxBatches batches, and the
//    master forks a fresh replacement that inherits the master's warm state
//    (autoload + bootstrap) via COW. This caps per-fork state accumulation —
//    e.g. Symfony bridge deprecation collectors — without paying the cost
//    of a one-batch-per-process model.
//    When $maxBatches = 0 we keep the original long-lived behaviour.
// ---------------------------------------------------------------------------
$forkChildForSlot = static function (int $slot) use (
    $childStdinStreams, $childStdoutStreams, $n, $workerMemoryLimit, $maxBatches, $perSlotDsn
): int {
    $pid = pcntl_fork();
    if ($pid === -1) {
        fwrite(STDERR, "worker_fork.php: pcntl_fork() failed for slot $slot\n");
        return -1;
    }
    if ($pid === 0) {
        posix_setpgid(0, 0);
        pcntl_signal(SIGTERM, SIG_DFL);
        pcntl_signal(SIGINT,  SIG_DFL);
        pcntl_signal(SIGHUP,  SIG_DFL);
        // Inject this worker's per-slot environment (worker id + DB clone DSN +
        // framework DATABASE_URL repoint) behind one contract. Re-applied
        // automatically on SIGCHLD respawn and K-batch/force_exit recycle —
        // both re-enter this closure with the same $slot.
        $__slotDsn = (isset($perSlotDsn[$slot]) && is_string($perSlotDsn[$slot]) && $perSlotDsn[$slot] !== '')
            ? $perSlotDsn[$slot]
            : null;
        (new \Proust\Worker\WorkerContext($slot, $__slotDsn, getenv('PROUST_EVENT_BRIDGE') === '1'))->apply();
        for ($j = 0; $j < $n; $j++) {
            if ($j !== $slot) {
                @fclose($childStdinStreams[$j]);
                @fclose($childStdoutStreams[$j]);
            }
        }
        // Mark THIS slot's pipes close-on-exec so any proc_open() from a
        // test method (PHPUnit's end-to-end fixtures intentionally exec()
        // child PHP processes to verify their own runner behaviour) spawns
        // a grandchild that does NOT inherit these FDs. Without CLOEXEC,
        // the grandchild reads/writes our slot's pipe and deadlocks the
        // master fork-pool — exactly the failure mode that hangs
        // phpunit-itself even after @runInSeparateProcess parity is fixed.
        if (function_exists('stream_set_close_on_exec')) {
            @stream_set_close_on_exec($childStdinStreams[$slot],  true);
            @stream_set_close_on_exec($childStdoutStreams[$slot], true);
        }
        runChild($childStdinStreams[$slot], $childStdoutStreams[$slot], $workerMemoryLimit, $maxBatches);
        exit(0);
    }
    @posix_setpgid($pid, $pid);
    return $pid;
};

// L3: single-process fast path. The runner sets --inline only for exactly one dispatch-safe slot,
// so run the per-batch loop in THIS master process — no pcntl_fork, no SIGCHLD/respawn machinery —
// matching vanilla's single-process model. Same runChild over slot 0's pipes a forked child uses.
if ($inline) {
    if (function_exists('stream_set_close_on_exec')) {
        @stream_set_close_on_exec($childStdinStreams[0], true);
        @stream_set_close_on_exec($childStdoutStreams[0], true);
    }
    // maxBatches=0 (unlimited) is LOAD-BEARING here: inline has NO master to fork a warm
    // replacement, so a voluntary recycle (WORKER_EXIT_VOLUNTARY_RECYCLE after $maxBatches batches)
    // would orphan the run — this process would just exit and the runner would synthesise
    // "worker crashed" errors for every still-queued test. The K-batch recycle is a fork-pool
    // memory/isolation device with no meaning without a respawner. Run ALL batches in this one
    // process, matching vanilla's single-process model (per the comment above). Passing $maxBatches
    // here was a latent bug: any inline suite exceeding K batches (e.g. a data-provider-heavy suite,
    // even within the method-count cap) lost every test past batch K.
    runChild($childStdinStreams[0], $childStdoutStreams[0], $workerMemoryLimit, 0);
    exit(0);
}

$slotPid    = array_fill(0, $n, 0);
$slotClosed = array_fill(0, $n, false);
$__log_phase('pre_fork');
for ($i = 0; $i < $n; $i++) {
    $slotPid[$i] = $forkChildForSlot($i);
    if ($slotPid[$i] === -1) exit(1);
    $childPids[] = $slotPid[$i];
}
$__log_phase('post_fork_all_children');

if ($maxBatches === 0) {
    // Long-lived mode: just wait for all children to exit (on Rust closing
    // their stdin) — no respawning. Closing master FDs here lets Rust's
    // reader see EOF when each child eventually exits.
    for ($i = 0; $i < $n; $i++) {
        fclose($childStdinStreams[$i]);
        fclose($childStdoutStreams[$i]);
    }
    foreach ($childPids as $pid) {
        pcntl_waitpid($pid, $status);
    }
    exit(0);
}

// Fork-server mode: install SIGCHLD handler to respawn children that exit
// voluntarily (after $maxBatches). A child exit with code 7 means "EOF on
// stdin, slot is closed" — we don't respawn then.
//
// CRITICAL: keep $childStdinStreams / $childStdoutStreams open in the master
// across the entire run. The kernel-level FDs underlie them; fresh forked
// children inherit those FDs only if the master still holds them.
pcntl_signal(SIGCHLD, function () use (
    &$slotPid, &$slotClosed, &$childPids, &$childStdoutStreams, $forkChildForSlot
): void {
    while (($deadPid = pcntl_waitpid(-1, $status, WNOHANG)) > 0) {
        $slot = array_search($deadPid, $slotPid, true);
        if ($slot === false) continue;
        $childPids = array_values(array_diff($childPids, [$deadPid]));
        $exitCode = pcntl_wifexited($status) ? pcntl_wexitstatus($status) : -1;
        // WORKER_EXIT_STDIN_EOF = child saw EOF on stdin; Rust closed this slot.
        if ($exitCode === WORKER_EXIT_STDIN_EOF) {
            $slotClosed[$slot] = true;
            $slotPid[$slot] = 0;
            continue;
        }
        // Distinguish a CRASH from a VOLUNTARY recycle. Voluntariness is now
        // EXPLICIT: only the reserved WORKER_EXIT_VOLUNTARY_RECYCLE code means
        // "K-batch limit / force_exit_after — fork a warm replacement, no
        // slot_died". A voluntary recycle has already emitted batch_done so
        // Rust's in_flight[slot] is None and the previous batch is fully
        // accounted for; sending slot_died here would race with the NEXT
        // batch's dispatch and make Rust attribute the death to the new
        // in-flight plan → synthetic errors for tests that ran fine.
        //
        // CRITICAL: a bare exit(0)/die() from a test/provider/teardown
        // mid-batch is NOT the reserved code, so it falls through to the crash
        // branch — exactly the fix. The child died before writing batch_done,
        // so in_flight[slot] is still Some on the Rust side; slot_died lets
        // Rust synthesise an error for the lost batch instead of hanging until
        // the watchdog. Signals (fatals 255, segfault 139, …) are crashes too.
        $isVoluntary = ($exitCode === WORKER_EXIT_VOLUNTARY_RECYCLE);
        $isCrash     = !$isVoluntary;
        if ($isCrash
            && isset($childStdoutStreams[$slot])
            && is_resource($childStdoutStreams[$slot])) {
            $signal = pcntl_wifsignaled($status) ? pcntl_wtermsig($status) : 0;
            @write_line($childStdoutStreams[$slot], [
                'slot_died' => true,
                'exit_code' => $exitCode,
                'signal'    => $signal,
            ]);
        }
        $newPid = $forkChildForSlot($slot);
        if ($newPid === -1) {
            $slotClosed[$slot] = true;
            $slotPid[$slot] = 0;
            continue;
        }
        $slotPid[$slot] = $newPid;
        $childPids[]    = $newPid;
    }
});

while (in_array(false, $slotClosed, true)) {
    pcntl_signal_dispatch();
    usleep(5_000);
}
foreach ($childPids as $pid) {
    @pcntl_waitpid($pid, $status, WNOHANG);
}
exit(0);

// ---------------------------------------------------------------------------
// Child: read newline-delimited BatchPlans in a loop, run classes per batch,
// stream TestOutcome JSON lines back and emit {"batch_done": true} between
// batches as a ready signal. Exit cleanly when Rust closes our stdin.
// ---------------------------------------------------------------------------
function runChild($stdinStream, $stdoutStream, string $memoryLimit, int $maxBatches = 0): void
{
    // Apply the worker-specific memory limit. This intentionally overrides
    // phpunit.xml's <ini name="memory_limit"> because that setting is designed
    // for short single-run PHP invocations, not long-lived workers that execute
    // thousands of tests in sequence. The value is controlled by --worker-memory-limit
    // (default "512M"); pass "-1" to restore unlimited behaviour.
    @ini_set('memory_limit', $memoryLimit);

    // Per-worker isolated temp directory. PHPUnit's @runInSeparateProcess
    // template builder calls tempnam(sys_get_temp_dir(), ...) and proc_open's
    // a sub-PHP against the resulting file; with several workers racing on
    // the same /tmp, two parallel separateProcess tests can stomp on each
    // other's template path, corrupt the sub-process's stdin, and hang the
    // worker waiting for output that will never come. Giving each child a
    // fresh TMPDIR/TMP/TEMP off its own PID dodges the collision entirely.
    // We also point sys_temp_dir at it (some PHP code reads the ini value
    // directly rather than the env var). The dir is best-effort cleaned by
    // a shutdown hook; tmpfs takes care of the rest when the container ends.
    $childPid = getmypid();
    $childTmp = sys_get_temp_dir() . "/proust-worker-" . $childPid;
    if (@mkdir($childTmp, 0700, true) || is_dir($childTmp)) {
        putenv("TMPDIR={$childTmp}");
        putenv("TMP={$childTmp}");
        putenv("TEMP={$childTmp}");
        $_ENV['TMPDIR']    = $childTmp;
        $_SERVER['TMPDIR'] = $childTmp;
        @ini_set('sys_temp_dir', $childTmp);
        register_shutdown_function(static function () use ($childTmp): void {
            // Recursive rmdir — best effort, ignore failures. Uses lstat
            // semantics (see proust_rmtree): a symlink a test wrote into
            // its TMPDIR is unlinked as a link, NEVER followed, so an external
            // target's contents are never deleted at worker exit.
            proust_rmtree($childTmp);
        });
    }

    // Current-batch state, captured by reference in the shutdown handler.
    // We reset both to empty after each clean batch so the handler doesn't
    // double-emit; it only fires for uncatchable fatals (E_COMPILE_ERROR etc.)
    // mid-batch, where these point to the class that killed us and the
    // remainder of its batch that we never reached.
    $currentClasses = [];
    $nextIdx        = 0;

    register_shutdown_function(function() use (&$currentClasses, &$nextIdx, $stdoutStream): void {
        while (ob_get_level() > 0) @ob_end_clean();
        // Name the fatal that killed us. A fataling child's stderr never
        // reaches the orchestrator (verified empirically: even a memory_limit
        // OOM leaves no trace in the captured output), but this handler DOES
        // run during fatal shutdown and error_get_last() still holds the
        // message — so ship it through the protocol pipe instead. Without
        // this, CI-only worker deaths are undiagnosable ("exit code 255" is
        // all the master sees). Segfaults/SIGKILL skip shutdown handlers
        // entirely, so a death WITHOUT a fatal suffix points at a signal.
        $fatal  = error_get_last();
        $suffix = '';
        $isFatalShutdown = $fatal !== null && in_array($fatal['type'],
                [E_ERROR, E_PARSE, E_CORE_ERROR, E_COMPILE_ERROR, E_USER_ERROR], true);
        if ($isFatalShutdown) {
            $suffix = sprintf(' (php fatal: %s in %s:%d)',
                $fatal['message'], $fatal['file'], $fatal['line']);
            // Redeclare-fatal breadcrumbs. NOTE: the include-registry check is
            // NOT discriminating (the dying include registers the file before
            // executing it, so it reads "yes" under both mechanisms) — the
            // decisive bit is `leaked`, measured in the master right after the
            // pre-warm loop: N>0 names classes that opcache_compile_file
            // declared into the master without any include-registry entry,
            // which every fork then inherits.
            if (str_contains($fatal['message'], 'Cannot redeclare')) {
                $prewarm = $GLOBALS['__proust_prewarm_count'] ?? 'off';
                $leaked  = $GLOBALS['__proust_prewarm_leaked'] ?? 'off';
                // The surgical bit: is the class we died redeclaring one the
                // master's pre-warm leaked? yes = mechanism confirmed.
                $thisLeaked = 'n/a';
                if (preg_match('/Cannot redeclare class (\S+)/', $fatal['message'], $m)) {
                    $thisLeaked = in_array(ltrim($m[1], '\\'),
                        $GLOBALS['__proust_prewarm_leaked_list'] ?? [], true)
                        ? 'yes' : 'no';
                }
                $suffix .= " [prewarm={$prewarm} leaked={$leaked} this-class-leaked={$thisLeaked}]";
            }
        }
        // Only the child can name an UNCATCHABLE FATAL (its text never reaches
        // the orchestrator otherwise), so emit a per-class row only then. A
        // shutdown WITHOUT a fatal means a test/provider/teardown called a bare
        // exit()/die() mid-batch: stay silent and let the master's `slot_died`
        // path be the single source — it alone knows the exit code and words
        // the cause ("exit code 0 … test code called exit/die mid-batch"). The
        // lost classes are still accounted for: Rust synthesises an Error per
        // lost method from `slot_died`. Emitting the generic "terminated before
        // this class could run" row here too would only race ahead of and mask
        // that breadcrumb (and, once dead-worker dedup lands, suppress it).
        if (!$isFatalShutdown) {
            return;
        }
        for ($i = $nextIdx; $i < count($currentClasses); $i++) {
            $class = (string)($currentClasses[$i]['class'] ?? '');
            if ($class === '') continue;
            @write_line($stdoutStream, [
                'class'       => $class,
                'method'      => '<class>',
                'dataset'     => null,
                'status'      => 'error',
                'message'     => 'worker process terminated before this class could run' . $suffix,
                'trace'       => null,
                'duration_ms' => 0.0,
            ]);
        }
    });

    $batchesProcessed = 0;
    while (true) {
        $line = fgets($stdinStream);
        if ($line === false || $line === '') {
            // EOF on stdin: Rust closed the pipe. Tell master via the reserved
            // EOF code so it knows this slot is closed (don't respawn).
            exit(WORKER_EXIT_STDIN_EOF);
        }
        $line = trim($line);
        if ($line === '') continue;

        $plan = json_decode($line, true);
        if (!is_array($plan) || !isset($plan['classes'])) {
            // Malformed line: still ack so master doesn't deadlock on us.
            write_line($stdoutStream, ['batch_done' => true]);
            continue;
        }

        $currentClasses = $plan['classes'];
        $nextIdx        = 0;

        foreach ($currentClasses as $i => $entry) {
            $file       = (string) ($entry['file']    ?? '');
            $class      = (string) ($entry['class']   ?? '');
            $methods    = (array)  ($entry['methods'] ?? []);
            // Optional stride row filter, e.g. {chunk_index: 2, total_chunks: 4}.
            // Applied uniformly to every data-provider method in this batch:
            // the runner emits one BatchClass per chunk when it wants to split
            // a heavy provider across workers.
            $rowFilter  = $entry['row_filter'] ?? null;
            if (!is_array($rowFilter)) { $rowFilter = null; }

            // Mark this class as "in progress" before touching it.
            // If exit()/fatal fires here, shutdown reports from $i onward.
            $nextIdx = $i;

            if ($file === '' || $class === '') {
                $nextIdx = $i + 1;
                continue;
            }

            if (!is_file($file)) {
                emitError($stdoutStream, $class, '<file>', "test file not found: $file");
                $nextIdx = $i + 1;
                continue;
            }

            ob_start();
            try {
                // Guard against E_COMPILE_ERROR "Cannot redeclare class": when
                // multiple fixture files share the same FQCN (e.g. PHPUnit's own
                // end-to-end fixtures), loading the second file in the same
                // long-lived worker process would be fatal. Skip require_once
                // when the class is already defined from a previous batch.
                if (!class_exists($class, false)) {
                    require_once $file;
                } else {
                    // Class already loaded from a different file. Emit errors for
                    // any methods in this batch that don't exist on the loaded
                    // definition so they appear in the report rather than being
                    // silently dropped by MethodPlanner's hasMethod filter.
                    $conflictMethods = array_unique(array_filter(
                        $methods, fn($m) => !method_exists($class, $m)
                    ));
                    foreach ($conflictMethods as $m) {
                        emitError($stdoutStream, $class, $m,
                            "method $m not found on $class (FQCN defined in multiple files; " .
                            "loaded from a different path — run with more workers to avoid conflicts)");
                    }
                    $methods = array_values(array_filter(
                        $methods, fn($m) => method_exists($class, $m)
                    ));
                    if (empty($methods)) {
                        ob_end_clean();
                        $nextIdx = $i + 1;
                        continue;
                    }
                }
                foreach ($entry['required_files'] ?? [] as $rf) {
                    if (is_string($rf) && is_file($rf)) {
                        require_once $rf;
                    }
                }
                if (!class_exists($class)) {
                    ob_end_clean();
                    emitError($stdoutStream, $class, '<class>',
                        "class $class not found after loading $file");
                    $nextIdx = $i + 1;
                    continue;
                }
                $isolated = !empty($entry['is_isolated']);
                // Optional per-batch tracing — set PROUST_TRACE_BATCHES=1
                // to write START/END markers to a per-slot file. After a
                // hang, the slot file whose last line is `START …` (no
                // matching END) names the class that froze the worker.
                static $traceFile;
                if (!isset($traceFile)) {
                    $dir = getenv('PROUST_TRACE_BATCHES');
                    // Accept "1" / "true" → default to /tmp; otherwise treat
                    // the env value as the destination directory so the
                    // caller can point traces at a bind-mounted host dir.
                    if ($dir === '1' || $dir === 'true') {
                        $dir = '/tmp';
                    }
                    $traceFile = ($dir && is_dir($dir))
                        ? rtrim($dir, '/') . '/proust-trace-' . getmypid() . '.txt'
                        : false;
                }
                if ($traceFile !== false) {
                    $t = sprintf('%.3f', microtime(true));
                    @file_put_contents($traceFile,
                        "$t START $class methods=" . count($methods) . "\n",
                        FILE_APPEND);
                }
                $outcomes = TestExecutor::runClass($class, $methods, $rowFilter, $isolated);
                if ($traceFile !== false) {
                    $t = sprintf('%.3f', microtime(true));
                    @file_put_contents($traceFile,
                        "$t END   $class outcomes=" . count($outcomes) . "\n",
                        FILE_APPEND);
                }
            } catch (\Throwable $e) {
                while (ob_get_level() > 0) ob_end_clean();
                emitError($stdoutStream, $class, '<class>',
                    'exception: ' . $e->getMessage(), $e->getTraceAsString());
                $nextIdx = $i + 1;
                continue;
            }
            ob_end_clean();

            foreach ($outcomes as $outcome) {
                write_line($stdoutStream, $outcome);
            }
            $nextIdx = $i + 1;
        }

        // Batch done cleanly: clear state so shutdown handler is a no-op
        // if the process exits naturally on the next read, then signal ready.
        $currentClasses = [];
        $nextIdx        = 0;
        write_line($stdoutStream, ['batch_done' => true]);

        // Fork-server mode: exit voluntarily after $maxBatches batches so
        // the master can fork a fresh child for the next batch. This bounds
        // per-fork state accumulation (e.g. Symfony bridge collectors).
        //
        // Per-batch override: when the runner marks a batch with
        // `force_exit_after` (set on batches whose class is is_stateful —
        // it registers stream wrappers, error handlers, etc.), we exit
        // immediately regardless of $batchesProcessed. The master forks
        // a fresh child for the next batch, guaranteeing zero cross-batch
        // pollution from this class's global side effects.
        // PROUST_NO_ISOLATION=1 disables the per-batch fresh-fork
        // for stateful classes — used only for A/B benchmarks to quantify
        // the parity-vs-perf trade-off.
        // NOTE: both recycle exits below fire AFTER the batch_done write above,
        // so Rust has already accounted for this batch (in_flight[slot] is
        // None). They use the reserved voluntary code so the master forks a
        // warm replacement WITHOUT emitting slot_died. A bare exit(0)/die()
        // from test code mid-batch never reaches here (it fires inside the
        // foreach above, before batch_done), so it correctly reads as a crash.
        if (!empty($plan['force_exit_after']) && !getenv('PROUST_NO_ISOLATION')) {
            exit(WORKER_EXIT_VOLUNTARY_RECYCLE);
        }
        if ($maxBatches > 0) {
            $batchesProcessed++;
            if ($batchesProcessed >= $maxBatches) {
                exit(WORKER_EXIT_VOLUNTARY_RECYCLE);
            }
        }
    }

    fclose($stdinStream);
    fclose($stdoutStream);
}

function emitError($stream, string $class, string $method,
                   string $msg, ?string $trace = null): void
{
    write_line($stream, [
        'class'       => $class,
        'method'      => $method,
        'dataset'     => null,
        'status'      => 'error',
        'message'     => $msg,
        'trace'       => $trace,
        'duration_ms' => 0.0,
    ]);
}
