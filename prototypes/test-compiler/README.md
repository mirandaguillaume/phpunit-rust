# Test-compiler prototype — hoist shared sub-trees, measure php8.4 wall-clock

A source-to-source **test compiler** for PHPUnit classes. It takes the e-graph's
finding — `crates/analyzer/src/reduce/egraph.rs::build_suite_egraph` reports the
structurally-shared call sub-trees of a suite (`top_targets`: an op like
`Carbon::parse` with multiplicity `m > 1`) — and emits a **compiled** version of the
class where each shared, **deterministic**, **immutable** sub-tree is computed **once**
in `setUpBeforeClass` (a `private static $pN`) and **referenced** by the test bodies.

The compiled PHP still runs entirely in PHP (no domain wall: bignum/dates stay
PHP-computed); it just does **less redundant work**. The gain is the real wall-clock
difference `time(original) − time(compiled)`, **measured under php8.4**, never estimated.

## The transformation (hoisting)

For each shared sub-tree (e-class of multiplicity `m > 1` that is a call/construction,
not a literal) that satisfies the side-conditions:

- declare `private static $pN;`
- `setUpBeforeClass()` computes `self::$pN = <sub-tree expression>` once;
- replace each safe occurrence of the sub-tree in the bodies with `self::$pN`.

Everything else (namespace, `extends`, every test method, every assertion) is left
byte-identical.

## Side-conditions of soundness (conservative — when in doubt, do NOT hoist)

1. **Deterministic.** The sub-tree contains no non-deterministic call
   (`now`, `today`, `rand`, `time`, `microtime`, `uniqid`, env/file reads,
   `Carbon::now()/today()`…). `Carbon::parse('<absolute date>')` is deterministic;
   `Carbon::now()` is a different op and is **never** a candidate.
2. **Immutable / read-only sharing.** A hoisted static is ONE object shared across ALL
   tests, so any in-place mutation would corrupt later tests. We hoist an occurrence
   **only** when it is an argument/value position or a property-read receiver
   (`->prop`). We **exclude**:
   - method-call receivers (`parse(...)->m(...)`) — the callee may mutate
     (`->utc()` == `setTimezone` in place on a mutable Carbon);
   - assignment RHS (`$x = parse(...);`) — the local alias may be mutated later
     (`$x->utc()`).
3. **Self-contained.** The sub-tree depends on no method-local variable, so it is
   identical across tests and hoistable into `setUpBeforeClass`.

## Correctness gate (mandatory)

The compiled file MUST produce **identical** PHPUnit results — same test count, same
assertions, same PASS/FAIL — as the original, under real php8.4 PHPUnit. Both targets
below were gated this way. The gate is not decorative: on a looser immutability rule it
caught **3 unsound hoists** in carbon (the `assign-rhs` alias-mutation case), which the
rule above then excludes.

## What is auto vs hand (prototype honesty)

- **Sharing detection** is the e-graph's (`egraph.rs`): which op sub-trees are shared
  and at what multiplicity/cost. That crate is unchanged by this prototype.
- **The hoisting emitter** is `scripts/compile_carbon.js` (carbon) and
  `scripts/gen_controlled.js` (controlled): automatic span-accurate source-to-source
  transforms that apply the side-conditions and emit valid PHP. The carbon compiler
  runs fully automatically over the real `DiffTest.php`; the controlled pair is
  generated automatically.
- The op-naming the emitter keys on (`Carbon::parse`, `<literal>`) is exactly the op
  naming `egraph.rs` materialises (`call_node` → `C::method`, `literal_node` →
  `str:'…'`), so the two agree on what "the same sub-tree" is.

## Measured results (php8.4, docker `proust-bench:php84`, median of runs)

### 1. Controlled fixture (`controlled/`) — proves compile→gain when sharing is expensive

`Heavy::build(1,2,3)` does ~8.5 ms of real deterministic CPU work (a 64-bit integer
mixing loop; no clock/rand/IO). 20 test methods each rebuild the identical object.

| | original | compiled | ratio |
|---|---|---|---|
| PHPUnit-internal test time | 244 ms | 17 ms | **14.4×** |
| full-process wall-clock (median of 7) | 226 ms | 72 ms | **3.14×** |

Result: `OK (20 tests, 20 assertions)` — **identical** in both. The original runs
`Heavy::build` 20×; the compiled runs it 1× (19 redundant builds × ~11 ms ≈ 227 ms saved
— matching the internal delta). When the shared sub-tree is expensive, the gain is net
and large. The full-process ratio is smaller because PHP+PHPUnit startup (~55 ms) is a
fixed floor that hoisting cannot remove.

### 2. Real carbon `DiffTest.php` (`carbon/DiffCompiledTest.php`) — the honest, modest case

229 `Carbon::parse('<literal>')` calls (48 distinct, 26 with multiplicity > 1) and 312
`Carbon::now()` calls. The compiler:

- **hoisted 11 literals** (each with ≥ 2 safe occurrences), replacing **84** parse
  calls with `self::$pN` (73 redundant parses eliminated: 84 → 11 computed once);
- **excluded** 116 method-call receivers and 26 assignment-RHS aliases (mutation risk);
- **never touched** the 312 `Carbon::now()` calls (non-deterministic).

| | original | compiled | ratio |
|---|---|---|---|
| PHPUnit-internal test time (median) | 103 ms | 103 ms | 1.00× |
| full-process wall-clock (median) | 178 ms | 177 ms | 1.006× |

Result: `OK (205 tests, 558 assertions)` — **identical** to the original baseline.

**The gain is negligible, and that is the measured truth.** A `Carbon::parse(literal)`
sub-tree costs ~19 µs (measured in isolation, 1000× loop). Eliminating 73 redundant
parses saves ~1.4 ms ≈ **1.37 %** of the 103 ms suite — below the run-to-run noise
(~5–10 ms), hence the ~1.0× end-to-end ratio. carbon's cost is dominated by the 312
`Carbon::now()` calls and 558 assertions, not by the shared parses we can soundly hoist.

## Two refinements evaluated — both REJECTED by measurement / the gate

### A. One-shot construction + per-test `clone` (for mutable objects)

Idea: a mutable Carbon can't be shared, but we can build it once and hand each use a
`clone` (isolation preserved, `clone` << construction). Micro-bench (php8.4, warm,
20000× loop): `Carbon::parse` ~1.0 µs, `Carbon::create` ~2.3 µs, **`clone` ~0.15 µs**
(~85 % cheaper than parse). So the mechanism is real and would let the 116 method-call
receivers + 26 alias occurrences (which the read-only compiler excludes) participate via
`(clone self::$protoN)`.

Built it (`scripts/compile_carbon_clone.js`): 26 protos, 207 parse→`(clone …)` rewrites.
**The identity gate REJECTED it — 4 failures.** Root cause (a real soundness finding):
`Carbon::parse('<no-zone literal>')` binds the **ambient default timezone at parse time**.
`setUpBeforeClass` runs *before* any per-test `setUp`, which does
`date_default_timezone_set('America/Toronto')` + `setTestNowAndTimezone(...)`. So a proto
parsed in `setUpBeforeClass` carries the **wrong timezone context**, and `clone` faithfully
preserves that wrong context (shallow copy) — e.g. a 4-hour offset drift and a
`PHPBug80974` mismatch. The refinement does not rescue timezone-context-dependent parses;
the read-only variant only passed because its 11 hoisted literals happened to be
timezone-insensitive at their use sites. Net: clone adds nothing sound here. Even had it
gated, the ceiling is ~100 × 0.86 µs ≈ 86 µs ≈ 0.08 % of the suite — noise.

### B. Treat `now()`/`today()` as deterministic when the clock is frozen

Idea: `Carbon::now()` is deterministic if `setTestNow(...)` froze the clock, so the 312
`now()` calls could be hoisted/one-shotted.

**Premise checked against the actual files — it does NOT hold for DiffTest.**
`AbstractTestCase::setUp()` does call `Carbon::setTestNowAndTimezone($now)`, but
`$now = Carbon::now()` — the **real wall-clock**, re-read in `setUp` **before every test**.
The freeze target therefore (1) changes between runs (non-deterministic) and (2) differs
per test; `wrapWithTestNow(...)` closures override it to yet other per-closure values.
Hoisting `now()` into `setUpBeforeClass` (one instant, once) would capture a moving target
and break every test expecting its own per-test now — the gate would reject it. So the
exclusion of `now()` is **correct for this file**; the refinement applies only to suites
that freeze to a FIXED literal (`setTestNow('2020-07-22 09:15')`), of which DiffTest has a
few inline cases but no suite-wide fixed freeze. now() stays excluded.

### Before → after

Both refinements leave the sound, gated result **unchanged**: carbon stays at
`OK (205 tests, 558 assertions)`, wall-clock ratio ~1.006×. The refinements are reported
here as measured dead-ends, not applied transforms — the strict identity gate is what
kept the prototype honest.

## Cost model (corrected): we memoize the WHOLE sub-tree

The memo is the **entire** shared sub-tree, not a single op:

- controlled: the whole `Heavy::build(1,2,3)` ≈ **8.5 ms** → 19 redundant copies = ~162 ms;
- carbon: the whole `Carbon::parse('2018-02-13 20:55:12.321456')` ≈ **19 µs** →
  73 redundant copies = ~1.4 ms.

Same mechanism, two orders of magnitude apart in sub-tree cost — which is exactly why
the controlled fixture wins big and carbon barely moves.

## Reproduce

```sh
# Controlled fixture (auto-generated, gated, timed):
node scripts/gen_controlled.js controlled
bash scripts/measure_controlled.sh        # correctness + wall-clock

# Carbon real file (auto-compiled, gated, timed):
node scripts/compile_carbon.js            # writes DiffCompiledTest.php next to DiffTest.php
bash scripts/run_carbon_orig.sh           # baseline: OK (205 tests, 558 assertions)
bash scripts/run_carbon_compiled.sh       # gate: must be identical
bash scripts/measure_carbon.sh            # wall-clock original vs compiled
```
