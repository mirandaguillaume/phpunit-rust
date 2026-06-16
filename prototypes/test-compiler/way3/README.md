# Way-3 — contextual-determinism hoisting (compile-pass v1)

The test compiler hoists a shared sub-tree into `setUpBeforeClass` only when it is
sound to compute it **once**. The hazard is **ambient context**: a no-zone
`Carbon::parse('2024-01-15 10:00')` / `new DateTime(...)` binds the **default
timezone at parse time**; a `now()` reads the **frozen clock**. Hoisting moves the
computation to a different point in the lifecycle — if the ambient context there
differs, the hoisted value is silently wrong.

**Way-3** decides hoistability by **context scope**, statically, on the source:

- a context setter (`date_default_timezone_set`, `setTestNow*`, `setlocale`) in
  `setUpBeforeClass` / bootstrap / a parent's `setUpBeforeClass` is **CLASS/GLOBAL**
  scope — stable through the `setUpBeforeClass` hoist slot;
- a setter in `setUp` / a parent's `setUp` is **PER-TEST** scope — the hoist slot
  runs *before* it, so the context there differs.

**Rule (v1, conservative on opaque calls):** a candidate call is opaque, so it could
read any ambient context. HOIST iff **no** ambient context is (re)established at
per-test scope; otherwise REFUSE. This statically refuses exactly the hoists the
earlier `clone`-refinement's *runtime* identity gate caught as failures.

## Three measured cases (php8.4, PHPUnit 11.5.55, docker `phpunit-rust-bench:php84`)

The shared sub-tree is `Heavy::parseExpensive('2024-01-15 10:00:00')`: it parses the
string in the **ambient** timezone, seeds a deterministic 31-bit mixing loop with the
resulting UTC timestamp, and burns ~9.5 ms of pure CPU. Result depends on **both** the
string and the ambient tz — Toronto → `3ecdec76`, UTC → `5829cd08` (a 5 h offset
shifts the seed). Two fixtures differ **only** in the scope of the tz setter; bootstrap
pins the baseline system tz to UTC.

| Case | tz scope | Way-3 | Parity | PHPUnit-internal time |
|------|----------|-------|--------|-----------------------|
| **A** `GlobalCtxTest` | class (`setUpBeforeClass`) | **HOIST** (mult 20→1) | `OK (20, 20)` — identical | **179 ms → 16 ms (11.2×)** |
| **B** `PerTestCtxTest` | per-test (`setUp`) | **REFUSE** (0 hoists) | `OK (20, 20)` — identical | 175 ms → 189 ms (no gain, sound) |
| **C** `PerTestCtxTest` | per-test | **NAIVE** (no Way-3) | **20 FAILURES** | hoisted value = UTC `5829cd08` ≠ Toronto `3ecdec76` |

Full-process wall-clock for A (median of 7, incl. docker+PHP+PHPUnit startup floor):
**0.52 s → 0.33 s (1.58×)** — the floor is fixed and un-hoistable; the *work* is what
collapses (19 redundant 9.5 ms computations → 1).

Case **C** is the load-bearing proof: the naive hoist computes under `setUpBeforeClass`'s
tz (UTC, the bootstrap baseline) because the per-test `setUp` that sets Toronto has not
run yet — producing the exact UTC hash. Way-3 **refuses this statically**; the refusal is
not cosmetic.

### Real carbon `DiffTest.php`

`AbstractTestCase::setUp` sets `date_default_timezone_set('America/Toronto')` **and**
`setTestNow(...)` — per-test scope `{tz, now}`. Way-3 inspects `DiffTest` + its parent,
finds **33** distinct shared candidates (`Carbon::parse(...)` up to multiplicity 48,
plus `Carbon::now(...)`, `setLocale`, `setTestNow`) and **REFUSES all of them** — 0
hoists, zero false greens. This is the static analogue of the `clone`-refinement's 4
runtime gate failures: same hazard, caught upstream.

**Honest limit of v1.** The earlier `compile_carbon.js` hoisted 11 literals that passed
the gate *by luck* — their use sites read tz-**independent** wall-clock components
(`->year`, `->month`, …). Conservative v1 refuses them too. Since carbon does not gain
either way (measured ~1.0×, its cost is `now()` + assertions, not the shared parses),
the trade costs nothing real. A **v2** with use-site context-sensitivity (a parse that
feeds only wall-clock-component reads is tz-independent) recovers those 11 soundly.

## Reproduce

```sh
# fixtures (bake the Toronto hash printed by a one-call probe):
node gen_fixtures.js 3ecdec76        # writes GlobalCtxTest.php + PerTestCtxTest.php into ../<dir>/tests

# compile each case:
node way3_compile.js          tests/GlobalCtxTest.php   out/GlobalCtxCompiledTest.php   # HOIST
node way3_compile.js          tests/PerTestCtxTest.php  out/PerTestCtxCompiledTest.php  # REFUSE
node way3_compile.js --naive  tests/PerTestCtxTest.php  out/PerTestCtxNaiveTest.php     # unsound baseline

# carbon decision (refuses all, per-test tz+now):
node way3_compile.js  <carbon>/tests/Carbon/DiffTest.php  out/DiffCompiledTest.php  <carbon>/tests/AbstractTestCase.php

# gate + measure under php8.4 (mount a clean phpunit vendor, e.g. brick-math's):
docker run --rm -v $PWD:/p -v <clean-vendor>:/p/vendor:ro -w /p phpunit-rust-bench:php84 \
  sh -c "php vendor/bin/phpunit -c phpunit.xml <file>"
```

## OSS offer scan (`way3_scan.js`) — where does the hoistable shape exist?

`way3_scan.js` resolves each test class's inheritance chain, classifies context
setters by scope, finds shared literal-arg call sub-trees, and decides HOIST/REFUSE
per suite. Across the 15 OSS smoke suites:

| suite | test classes | per-test-ctx | shared distinct | HOIST | REFUSE | occHoist | maxMult |
|-------|-------------:|-------------:|----------------:|------:|-------:|---------:|--------:|
| brick-money | 41 | 0 | 53 | 53 | 0 | 278 | 17 |
| carbon | 120 | 110 | 208 | 34 | 174 | 109 | 4 |
| brick-math | 12 | 0 | 20 | 20 | 0 | 66 | 7 |
| php-semver | 6 | 0 | 19 | 19 | 0 | 58 | 12 |
| phpgeo / phpgeo2 | 33 | 0 | 19 | 19 | 0 | 38 | 2 |
| mathphp, moneyphp, fraction, … | — | 0 | 0 | 0 | 0 | 0 | 0 |
| **TOTAL** | **480** | — | **338** | **164** | **174** | — | — |

The hoistable shape **exists** (164 sound HOISTs), but every shared literal-arg
sub-tree is a **cheap value-object construction** — measured per-call cost:
`Currency::of('EUR')` 0.09 µs, `BigInteger::of('1e1000')` 0.73 µs (a 1001-digit
integer, stored lazily as a string), `BigRational::of('1/3')` 1.79 µs,
`Version::parse('5.2.3')` 0.44 µs. Aggregate saving per suite (Σ (mult−1) × cost) is
an **upper bound of ~0.5 ms** (brick-money) down to tens of µs — **below run-to-run
noise**, so every primitive OSS suite stays ~1.0×. carbon is dominated by per-test
`{tz,now}` (110/120 classes) → 174 REFUSE, confirming the prototype's measured ~1.0×.

**Conclusion: literal-arg sharing is real but cheap.** The gain needs an *expensive*
shared sub-tree — and those are built from variables in multi-statement `setUp`
bodies, not `Class::method('literal')`, so the literal detector misses them.

## setUp-splitter (`way3_setup.js`) — the expensive shape

The most expensive, highest-multiplicity shared computation in any suite is the
`setUp()` body itself: it runs once **per test**. `way3_setup.js` hoists the
deterministic, immutable prefix of `setUp` into `setUpBeforeClass` (run once),
keeping the per-test-mutable remainder. Two soundness gates:

1. **Determinism + context (Way-3):** no per-test ambient context, no non-deterministic
   call in the factory.
2. **No mutation:** no test method mutates the shared object (a non-reader
   `$this->P->m(...)`, a write, or a reassignment). Else REFUSE — per-test rebuild stays.

`HeavySchema::compile()` does ~10.8 ms of deterministic CPU (the "compile a schema /
boot an EntityManager" shape). 20 tests each rebuild it in `setUp`.

| case | fixture | Way-3 | parity | PHPUnit-internal |
|------|---------|-------|--------|------------------|
| **A** `SchemaReadTest` | read-only | **HOIST** setUp→setUpBeforeClass | `OK (20, 40)` identical | **110 ms → 12 ms (9.2×)** |
| **B** `SchemaMutateTest` | mutates (`addTable`) | **REFUSE** | `OK (20, 20)` identical | no spurious gain |
| **C** `SchemaMutateTest` | mutates, **naive** | hoist anyway | **19 FAILURES** | shared schema accumulates: counts 21,22,23… ≠ 4 |

Full-process wall-clock A (median of 7): **0.46 s → 0.34 s (1.35×)** — startup floor
dominates; the *work* collapses 20×→1×.

Case **C** is the load-bearing proof for the second gate: hoisting a mutated fixture
lets `addTable()` calls accumulate on the shared object, so later tests see 21/22/23
tables instead of 4. Way-3 refuses this **statically** via mutation analysis. This is
the expensive-shape analogue of the timezone result — same structure, different gate.

## Status

This JS demonstrator proves Way-3's two soundness gates (context-scope **and**
mutation) end-to-end, measured and gated, on both the cheap literal shape and the
**expensive `setUp` shape** where the gain is real (9.2×). The OSS offer is quantified:
literal-arg sharing is cheap (~1.0×); the lever pays on heavy deterministic `setUp`
fixtures. The next step is porting the analysis into the Rust engine
(`crates/analyzer/src/reduce/egraph.rs`) as side-conditions on the suite-wide
compression plan, then driving the warm-master `$GLOBALS` precompute from it.
