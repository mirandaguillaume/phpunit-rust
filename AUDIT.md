# Senior Rust Audit — phpunit-rust

**Date:** 2026-06-02 · **Reviewed at:** `feat/dirty-impact-selection` · **Scope:** 4-crate Cargo workspace (`runner`, `analyzer`, `discovery`, `mock_baker`), ~15,757 Rust LOC + the PHP worker boundary (`php/`).

## Methodology

This audit was produced by fanning out **10 dimension specialists** (fork/unsafe safety, orchestration, IPC protocol, error robustness, parsing, concurrency, analyzer core, idioms/API, build/CI/supply-chain, architecture), then **adversarially re-verifying every non-trivial finding against the actual code** (a second agent tried to *refute* each claim and re-read the cited lines). 60 raw findings → **35 confirmed, 17 partial (real but mis-rated/mis-located), 2 refuted, 6 informational**. The two highest-impact findings were additionally hand-verified by the reviewer against the live repo and git state.

## Verdict

This is **competent, frequently excellent systems code** — the FD lifecycle in `fork_pool.rs`, the lost-batch crash-recovery, the parity-vs-performance reasoning in comments, and the analyzer's module hygiene are all above the bar for the genre. The project is held back by **operational hygiene gaps, not core code quality**: a committed live secret, no CI for a workspace that ships `unsafe` process plumbing and stakes its reputation on exact parity, one genuine liveness defect (unbounded hang), and a cluster of silent-drop paths that quietly undermine the stated correctness goal.

## Scorecard

| Severity | Count | Headline items |
|---|---:|---|
| 🔴 Critical | 2 | Committed live OAuth token; unbounded hang on a stuck worker |
| 🟠 High | 6 | No CI / no parity regression harness; non-UTF-8 outcome drop; unbounded AST recursion (stack overflow); cache keys omit config |
| 🟡 Medium | 10 | Drop signals a reaped PID; tree-sitter ERROR nodes ignored; coverage walker skips loops/switch/try; Unix-only but not `cfg(unix)`-gated; dispatcher untested |
| ⚪ Low | ~32 | Allocation hotspots; no lints/`#[must_use]`; dead code; `anyhow` in library crates; 2285-line monolith |

---

## 🔴 Critical

### C1 — Live Anthropic OAuth token committed to the repository
**`.envrc:1`** · confirmed (hand-verified)

`git ls-files .envrc` → `.envrc` (tracked), present in `HEAD` (commit `9fb5558`), **not** in `.gitignore`. Line 1 is `export CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-…` (a well-formed 108-char `sk-ant-oat01` Claude Code token, redacted here).

- **Impact:** Anyone with read access to the repo — or anyone at all if it is ever pushed to a public remote — can impersonate the account. The secret is in git **history**, so deleting the line does not remediate it.
- **Fix (in order):** (1) **Revoke/rotate the token now** in the Anthropic console. (2) `git rm --cached .envrc`, add `.envrc` to `.gitignore`, commit a `.envrc.example` with placeholders. (3) Purge from history with `git filter-repo` / BFG. (4) Add a `gitleaks`/`trufflehog` pre-commit hook so this cannot recur.

### C2 — No inactivity watchdog: a stuck-but-alive worker hangs the run forever
**`crates/runner/src/runner.rs:270-273`** · confirmed

The dispatch loop is `while live_readers > 0 { let ev = match rx.recv() { Ok(e) => e, Err(_) => break } … }` — an **unbounded blocking `recv()`**. Per-slot reader threads block in `reader.read_line()`. Recovery exists only for a worker that *dies* (`SlotDied` via the master's `SIGCHLD` handler) or closes its pipe (`Eof`). If a child is **alive but stuck** (an infinite loop, a blocked `setUp`, or a test that spawns a sub-process which never returns — exactly PHPUnit's own `@runInSeparateProcess`/proc_open fixtures), it emits no outcome, no `batch_done`, no EOF, and no `SIGCHLD` ever fires.

- **Impact:** The entire run deadlocks with no progress and no timeout. PHPUnit's own suite cannot be run to completion. A CI job would hang until its outer timeout. This is the same defect the README candidly documents at `README:285-306` and the ROADMAP designs a fix for (8.1).
- **Fix:** Replace `rx.recv()` with `rx.recv_timeout(tick)`, track per-slot last-progress `Instant`s (the scaffolding already exists via `slot_batch_start`), and `SIGKILL` a slot that produces nothing for N seconds — the master already forks a replacement on `SIGCHLD`, and the existing `SlotDied` lost-batch path then takes over, converting an unbounded hang into a bounded, recoverable event.

---

## 🟠 High

### H1 — No CI pipeline at all
**`.github` (absent)** · confirmed

No `.github/workflows`, no `.gitlab-ci.yml`, no `.travis.yml` — nothing enforces `cargo build`/`test`/`clippy`/`fmt` on a 15.7k-LOC workspace that contains raw `unsafe` fork-time code (`fork_pool.rs:68,118`) and `libc::kill` signal handling, and that parses untrusted PHP/XML.
- **Fix:** GitHub Actions running `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, `cargo fmt --check`, plus `cargo deny`/`cargo audit`. Ideally a PHP 8.1–8.4 matrix for the discovery/parity integration tests.

### H2 — The headline parity claim has zero automated regression harness
**`README.md:17`** · confirmed

The README's value proposition — "the `Tests: N` line we report should match `./vendor/bin/phpunit`" — is backed only by a **hand-maintained scoreboard**. A regression in discovery, provider enumeration, or the `TestExecutor` lifecycle that silently changes counts would not be caught until someone re-runs benches manually.
- **Fix:** A CI job that vendors 2–3 small pinned suites (guzzle-psr7, php-parser), runs vanilla phpunit *and* phpunit-rust, and asserts equal `Tests:`/`Assertions:` counts. This converts the scoreboard from a claim into a contract.

### H3 — `json_encode` without `JSON_INVALID_UTF8_SUBSTITUTE` silently drops outcomes
**`php/worker_fork.php:638`** (also 493, 520, 648, 682) · confirmed

PHP's `json_encode` returns `false` — not a string — on any invalid UTF-8 byte. Exception messages and stack traces from arbitrary test code routinely contain raw bytes (binary assertion diffs, latin-1 fixtures). `false . "\n"` coerces to a bare `"\n"`, so the **outcome line is silently lost** → a test-count parity violation, which is the project's stated correctness goal.
- **Fix:** Pass `JSON_INVALID_UTF8_SUBSTITUTE` (and ideally `JSON_PARTIAL_OUTPUT_ON_ERROR`) on every worker-stdout `json_encode`, centralize the write in one helper, and emit a sanitized hand-built fallback line if encoding still fails.

### H4 — Type-walker AST recursion has no depth limit → stack overflow on untrusted PHP
**`crates/analyzer/src/types/walker.rs:197`** · confirmed

`walk_expression`/`walk_statement_ctx` recurse with **no depth guard** — notably inconsistent with the sibling `concrete/expr.rs:81` evaluator, which *does* guard (`max_depth=100`). Deeply nested parentheses, long binary chains, or nested array literals in untrusted source overflow the native stack. It runs under rayon worker threads (default stack size, no `stack_size` override), so the overflow aborts the whole process.
- **Fix:** Add `depth`/`max_depth` to `WalkerCtx`, bail to `Type::Mixed` past a few hundred levels — mirroring `concrete::expr::Context`.

### H5 — Trace/result cache keys omit project config → stale coverage
**`crates/analyzer/src/cli/analyze.rs:291`** · confirmed

`trace_v2_key` = `class+method` only; the result fingerprint hashes only file path/size/mtime; neither encodes `cfg.source_includes/excludes`. But `opacity::decide` routes Trace-vs-Opaque off those boundaries. Editing `<source>` in `phpunit.xml` (or pointing `--config` elsewhere) changes the coverage map with **no PHP-file mtime change**, so both caches report "still valid" and return coverage computed under the old boundaries — silently incorrect.
- **Fix:** Fold a hash of the resolved `ProjectConfig` (root + sorted includes/excludes/suites) into the `CacheStore` namespace or into both keys.

### H6 — Dispatcher hang (architecture framing of C2)
**`crates/runner/src/runner.rs:271`** · confirmed — same root cause as **C2**, surfaced independently by the architecture reviewer. Recovery handles worker *death* but not *hang*. See C2 for the fix.

---

## 🟡 Medium (selected detail)

| # | Finding | Location | Note |
|---|---|---|---|
| M1 | **Statement walker ignores `foreach`/`for`/`while`/`switch`/`match`/`try` bodies** → call sites in those constructs emit no events → systematically under-reported coverage (parity/clover correctness). | `walker.rs:1217` | confirmed |
| M2 | **`lookup_return_type` does an O(n) class scan per call-site** despite an O(1) `find_class_reflection` index used elsewhere — re-lowercases every FQCN per lookup; contradicts the perf work in `093c9d2`. | `walker.rs:827` | confirmed, zero-risk fix |
| M3 | **`Drop` sends `SIGTERM`/`SIGKILL` to an already-reaped PID.** `runner::run` calls `pool.wait()` (`runner.rs:425`) before the pool is dropped (`main.rs:677`); the kernel may recycle that PID in the window, so Drop's unconditional `libc::kill` (`fork_pool.rs:254-255`) can signal an innocent process. | `fork_pool.rs:254` | confirmed; downgraded high→medium (harm bounded, but it's a textbook PID-reuse hazard). **Fix:** track a `reaped: bool` and skip the raw kill when set. |
| M4 | **Parsers never check tree-sitter `ERROR` nodes.** `parse()` returns `Some(tree)` for malformed PHP; a syntax error (or unsupported grammar) yields a partial tree → silently dropped tests. | `discovery/src/lib.rs:242`; `mock_baker/src/lib.rs:89,122,267` | confirmed. **Fix:** check `root_node().has_error()`, warn or delegate the file to real PHPUnit. |
| M5 | **`mock_baker::walk` is unbounded recursion** (one frame per AST level) over arbitrary `--bake-mocks` source → stack overflow on deeply nested PHP. | `mock_baker/src/lib.rs:210` | confirmed. **Fix:** explicit-stack iteration or depth cap (the discovery body-walk already does this). |
| M6 | **Runner is Unix-only by hard dependency but not `cfg(unix)`-gated** → `cargo build` *fails to compile* on Windows (not just fails to run), blocking even the platform-independent crates. README frames Windows as merely "not supported." | `fork_pool.rs:11` | confirmed. **Fix:** `#![cfg(unix)]` + a clear `compile_error!`, and document that `PR_SET_PDEATHSIG` is Linux-only (weaker orphan reaping on macOS). |
| M7 | **The dispatcher and *all* failure-recovery paths have no test coverage.** The 9 unit tests target pure planning fns; `run_with_profiler`, `SlotDied`, the `Eof` crash path, `stop_on` draining, and affinity dispatch are exercised only by manual benching. | `runner.rs:134` | confirmed. **Fix:** drive `run_with_profiler` against a scriptable fake-worker stub that emits controllable outcome/crash sequences. |
| M8 | **README's "delegate to the project's PHPUnit" is actually a reimplementation** of PHPUnit's runner. `TestExecutor.php`/`MethodPlanner.php` explicitly have "no knowledge of PHPUnit's TestRunner/Facade/Configuration" and re-implement lifecycle ordering, `@requires`, `@depends`, providers, `expectException`. This is the true source of version fragility (PU9 broken, faker −14, doctrine +9). | `php/src/TestExecutor.php:9` | confirmed; doc/maintainability. **Fix:** reframe the docs honestly and treat each PHPUnit major as a tracked lifecycle contract. |
| M9 | **State-isolation is a narrow syntactic heuristic** (15 hardcoded global APIs, walks a class's own + inherited chain but not trait methods, called helpers, or static-property accumulators) — the load-bearing correctness risk of the long-lived-worker design, which the project admits leaks (guzzle-psr7). | `discovery/src/lib.rs:721` | partial (parent-class methods *are* followed via `chain_is_stateful:1247`; list is 15 not 16). **Fix:** document blind spots; add a per-class force-isolate opt-in. |
| M10 | **Default discovery silently drops every test in an unparseable/non-UTF-8 file** (`parse_file_classes(p).unwrap_or_default()`); PHP would happily run those tests. | `discovery/src/lib.rs:1502` | partial (mis-dimensioned as a panic; it's a silent parity bug). **Fix:** `from_utf8_lossy` + a visible "skipped N undiscoverable files" counter. |

---

## ⚪ Low (grouped)

**Allocation hotspots** (ROADMAP already flags FQCN interning): per-method/per-batch `HashSet<String>` fingerprint cloned ≥3× along `build_queue`→dispatch (`runner.rs:583-588,594-596,246,308`); `mk_plan` builds owned `(String,String)` keys per lookup (`:594`); `cfg.defines`/`cfg.autoload` deep-cloned into every `BatchPlan`, `cfg.bootstrap` cloned though always `None` (`:600-602`). → intern FQCNs as `Arc<str>`/IDs; share config via `Arc`.

**Tooling/build:** no `rustfmt.toml`/`clippy.toml`/`deny.toml`, no `[workspace.lints]`, no crate-level `#![deny]`/`#![forbid]`, no `#![deny(unsafe_op_in_unsafe_fn)]` (`Cargo.toml`); no `[profile.release]` → defaults to `panic=unwind` (relevant in forked children), no LTO/strip; no MSRV; `tree-sitter`/`tree-sitter-php` pinned by literal `"0.22"` in 3 manifests (drift risk — make them workspace deps); Docker bases use floating tags; bench harness has hardcoded absolute paths.

**API / idioms:** zero `#[must_use]` on pure value-returning fns (`Report::count/passed/is_success`, `StopOn::*`); `discovery`/`mock_baker` library crates expose `anyhow::Result` (context-loss, no typed errors for downstream — prefer `thiserror`); `discovery/src/lib.rs` is a **2285-line monolith** mixing 6 concerns (contrast the analyzer's tidy module tree).

**Dead / vestigial code:** `slot_busy` written on every dispatch but never read (`runner.rs:199`); `is_dispatch_safe` threaded through two structs, read only inside `#[cfg(test)]`; `method_weight` is a 3-arg fn that unconditionally returns `1` (`:439`); `chunk_by_class` + a legacy parallel discovery API are `pub` but only hit by tests/examples; `PhpForkPool::into_readers` is dead.

**Robustness tail:** non-atomic cache writes (`std::fs::write` from concurrent rayon threads — crash/corrupt mid-write; `cache/store.rs:34`); cache GC implemented but never wired in → LRU degrades to FIFO on `noatime` (`cache/gc.rs:24`); `BufReader::read_line` treats any non-UTF-8 byte as hard `Err` and tears the slot down as EOF (`runner.rs:191`); `phpunit.xml` bootstrap/testsuite/exclude paths resolved without project-root containment (`main.rs:341`); reader threads not joined on the `write_batch` error path (benign — process exiting); `pipe()`+`fcntl(CLOEXEC)` is a non-atomic two-step (only theoretically racy here — no concurrent spawn occurs); transient `write_batch` error aborts the whole run and discards collected results (`runner.rs:309`); worker lines failing JSON parse are silently discarded (`runner.rs:185`); `slot_died` recovery emits one error per method, under-counting data-provider rows vs vanilla (`runner.rs:350`).

---

## Refuted / corrected (transparency)

The adversarial pass **refuted 2** findings and **downgraded 17** — evidence the verification did real work, not rubber-stamping:
- ❌ *"`provider_enum.rs` deadlocks: full stdin written before stdout drained"* — refuted: the classic pattern is present, but the enumeration payload is bounded well under the 64KB pipe buffer in practice, so it does not deadlock at realistic suite sizes (a *large-suite* variant survives as a low-severity hardening note).
- ❌ *"`mock_baker` abstract-class detection splits on substring `class` and misclassifies"* — refuted on mechanism and reachability.
- ⬇️ The `fork_pool.rs` Drop-kills-reaped-PID issue was downgraded high→medium (harm bounded); several "panic" findings were re-dimensioned as silent-parity bugs, not crashes.

---

## Genuine strengths (balanced view)

- **`fork_pool.rs` is exemplary systems code.** Every raw FD is wrapped in `File` exactly once (no double-close), early-return-after-partial-pipe auto-closes, CLOEXEC partitions Rust-facing vs PHP-facing ends correctly, `fcntl` errors are propagated, and `PR_SET_PDEATHSIG` + `SIGTERM`-then-`SIGKILL` Drop is a thoughtful shutdown story. **Crucially, Rust never calls `fork()` itself** — `pcntl_fork()` happens inside the separately `exec`'d PHP master, so the classic "rayon threads live across `fork()` → locked malloc arena" hazard *does not exist* in the Rust address space.
- **Crash recovery preserves parity.** `SlotDied`/`Eof` paths synthesize per-method error outcomes so a *dying* worker neither silently loses tests nor hangs the dispatcher. (The gap is *hang*, not *death* — C2.)
- **Defensive parsing.** Every untrusted-source string slice derives its offset from an ASCII-delimiter `find()` (no UTF-8 boundary panics); the discovery body-walk is iterative; class-graph BFS is `visited`-guarded and depth-bounded (no hang on cyclic `extends`); `WalkDir` keeps `follow_links(false)` (no symlink escape); `quick-xml` doesn't expand DTD entities (no billion-laughs); mock baking writes only into a `TempDir`.
- **Disciplined error handling** in the runtime path: of ~324 `unwrap/expect/panic`, only ~12 are reachable at runtime, and each is provably guarded or unreachable — unusually clean for a tool that eats untrusted input. `thiserror` for the analyzer's structured errors, `anyhow` for the binary, is an idiomatic split.
- **Sound concurrency model:** explicit `build_global()` before any `par_iter`, single-owner scheduler state (no shared atomics needed), correct mpsc fan-in termination, order-preserving parallel discovery (counts are deterministic).
- **Candid documentation** and high-quality "why" comments throughout, especially around the parity↔performance tradeoffs.

---

## Prioritized remediation roadmap

1. **Today:** Rotate the leaked token (C1); remove `.envrc` from tracking + history.
2. **This week:** Add the inactivity watchdog (C2/H6) — it's the one defect that makes the tool unusable on real suites; add a CI workflow with build/test/clippy/fmt (H1) and a small parity-count job (H2).
3. **Next:** `JSON_INVALID_UTF8_SUBSTITUTE` on worker output (H3); depth guards on `walker.rs` and `mock_baker::walk` (H4/M5); fold config into cache keys (H5).
4. **Hardening:** tree-sitter `has_error()` handling + surface skipped/undiscoverable files (M4/M10); `cfg(unix)` gate + `[profile.release]` + workspace lints; `Arc`/interning for the allocation hotspots; tests for the dispatcher recovery paths (M7).
5. **Strategic:** reframe the "delegation" narrative and treat each PHPUnit major as a versioned lifecycle contract (M8); strengthen or document the state-isolation heuristic (M9).
