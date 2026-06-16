#!/usr/bin/env node
// Way-3 setUp-splitter — hoist the DETERMINISTIC, IMMUTABLE prefix of setUp() into
// setUpBeforeClass so it runs ONCE instead of once-per-test. This targets the
// EXPENSIVE shared shape (EntityManager / schema / fixture build) that the
// literal-arg detector misses.
//
// Two soundness gates on a candidate `$this->P = <factory>;` in setUp:
//   1. Determinism + context (Way-3): no per-test ambient context setter could
//      change the factory's result (tz/now/locale), and the RHS has no obvious
//      non-deterministic call (now/rand/time/uniqid/microtime).
//   2. No mutation: NO test method (nor tearDown) mutates $this->P. A mutation is
//      a non-reader method call `$this->P->m(...)`, a property/array write, or a
//      reassignment. If the shared object were mutated, later tests would observe
//      it — so we REFUSE and keep the per-test rebuild.
//
// HOIST iff both gates pass; else REFUSE (sound: per-test rebuild preserved).
// --naive ignores gate 2 (the unsound baseline, for contrast).
//
// usage: way3_setup.js [--naive] <src> <out> [parent ...]
const fs = require('fs');

const NAIVE = process.argv.includes('--naive');
const a = process.argv.slice(2).filter(x => x !== '--naive');
const [SRC, OUT, ...PARENTS] = a;
if (!SRC || !OUT) { console.error('usage: way3_setup.js [--naive] <src> <out> [parent ...]'); process.exit(1); }
const s = fs.readFileSync(SRC, 'utf8');

function matchBrace(str, open) { let d = 0; for (let i = open; i < str.length; i++) { const c = str[i]; if (c === '{') d++; else if (c === '}') { d--; if (d === 0) return i; } } return -1; }
function methodSpan(str, name) {
  const re = new RegExp(`function\\s+${name}\\s*\\(`, 'g'); let m;
  while ((m = re.exec(str)) !== null) {
    const after = str.slice(m.index, m.index + 30);
    if (name === 'setUp' && /setUpBeforeClass/.test(after)) continue;
    const open = str.indexOf('{', m.index); const close = matchBrace(str, open);
    return { sig: m.index, open, close, body: str.slice(open + 1, close) };
  }
  return null;
}
// all method bodies EXCEPT setUp/setUpBeforeClass/tearDownAfterClass — i.e. the tests + tearDown
function testBodies(str) {
  const out = []; const re = /function\s+(\w+)\s*\([^)]*\)\s*(?::\s*\w+\s*)?\{/g; let m;
  while ((m = re.exec(str)) !== null) {
    const name = m[1];
    if (/^(setUp|setUpBeforeClass|tearDownAfterClass)$/.test(name)) continue;
    const open = str.indexOf('{', m.index); const close = matchBrace(str, open);
    out.push({ name, body: str.slice(open + 1, close) });
  }
  return out;
}

// ---- context (Way-3) ----
const CTX = { tz: /\bdate_default_timezone_set\s*\(/, now: /\b(setTestNow|setTestNowAndTimezone)\s*\(/, locale: /\bsetlocale\s*\(/ };
function ctxIn(t) { const o = {}; for (const k in CTX) o[k] = !!t && CTX[k].test(t); return o; }
let perTestCtx = { tz:false, now:false, locale:false };
function accPerTest(text) { const su = methodSpan(text, 'setUp'); if (su) { const c = ctxIn(su.body); perTestCtx = { tz:perTestCtx.tz||c.tz, now:perTestCtx.now||c.now, locale:perTestCtx.locale||c.locale }; } }
accPerTest(s); for (const p of PARENTS) if (fs.existsSync(p)) accPerTest(fs.readFileSync(p, 'utf8'));
const anyPerTestCtx = perTestCtx.tz || perTestCtx.now || perTestCtx.locale;

const NONDET = /\b(rand|mt_rand|random_int|time|microtime|uniqid|hrtime|date|now|today)\s*\(/i;

// ---- candidate setUp assignments ----
const su = methodSpan(s, 'setUp');
const tests = testBodies(s);
const READER = /^(get|is|has|to|as|with|count|equals|compare|fingerprint|tables|tableCount|toArray|jsonSerialize)/i;

function mutates(prop) {
  // scan tests + tearDown for a mutation of $this->prop
  const p = prop.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const reCall = new RegExp(`\\$this->${p}->(\\w+)\\s*\\(`, 'g');
  const reWrite = new RegExp(`\\$this->${p}(\\[[^\\]]*\\]|->\\w+)?\\s*=(?!=)`, 'g');
  for (const t of tests) {
    let m;
    while ((m = reCall.exec(t.body)) !== null) if (!READER.test(m[1])) return { test: t.name, kind: `->${m[1]}()` };
    reWrite.lastIndex = 0;
    while ((m = reWrite.exec(t.body)) !== null) return { test: t.name, kind: 'write/reassign' };
  }
  return null;
}

const decisions = [];
let newSetUpBody = su ? su.body : '';
if (su) {
  const reAssign = /\$this->(\w+)\s*=\s*([^;]+);/g; let m;
  while ((m = reAssign.exec(su.body)) !== null) {
    const prop = m[1], rhs = m[2].trim(), full = m[0];
    // gate 1: determinism + context
    const nondet = NONDET.test(rhs);
    const ctxUnsafe = anyPerTestCtx; // conservative: opaque factory could read per-test context
    // gate 2: mutation
    const mut = mutates(prop);
    const hoist = NAIVE ? true : (!nondet && !ctxUnsafe && !mut);
    let reason;
    if (NAIVE) reason = 'NAIVE: hoisted ignoring mutation';
    else if (nondet) reason = 'REFUSE: non-deterministic RHS';
    else if (ctxUnsafe) reason = `REFUSE: per-test ambient context (${Object.keys(perTestCtx).filter(k=>perTestCtx[k]).join(',')})`;
    else if (mut) reason = `REFUSE: mutated by ${mut.test} (${mut.kind})`;
    else reason = 'HOIST: deterministic, context-stable, never mutated';
    decisions.push({ prop, rhs, full, mult: tests.length, hoist, reason });
    if (hoist) newSetUpBody = newSetUpBody.replace(full, `/* hoisted: ${prop} */`);
  }
}

const hoisted = decisions.filter(d => d.hoist);

// ---- emit ----
const origClass = (s.match(/class\s+(\w+)\s+extends/) || [])[1];
const newClass = (origClass || 'X').replace(/Test$/, '') + (NAIVE ? 'NaiveTest' : 'CompiledTest');
let out = s;

// replace setUp body (drop hoisted assignments)
if (su && hoisted.length) out = out.slice(0, su.open + 1) + newSetUpBody + out.slice(su.close);

// recompute spans on the mutated string for property-reference rewrite
for (const d of hoisted) {
  const p = d.prop.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  out = out.replace(new RegExp(`\\$this->${p}\\b`, 'g'), `self::$s_${d.prop}`);
}

// rename class
out = out.replace(new RegExp(`class\\s+${origClass}\\b`), `class ${newClass}`);

if (hoisted.length) {
  const decls = hoisted.map(d => `    private static $s_${d.prop};`).join('\n');
  const assigns = hoisted.map(d => `        self::$s_${d.prop} = ${d.rhs};`).join('\n');
  const sub = methodSpan(out, 'setUpBeforeClass');
  if (sub) out = out.slice(0, sub.close) + assigns + '\n    ' + out.slice(sub.close);
  else {
    const co = out.indexOf(`class ${newClass}`), br = out.indexOf('{', co);
    const meth = `\n    public static function setUpBeforeClass(): void\n    {\n        parent::setUpBeforeClass();\n${assigns}\n    }\n`;
    out = out.slice(0, br + 1) + meth + out.slice(br + 1);
  }
  const co = out.indexOf(`class ${newClass}`), br = out.indexOf('{', co);
  out = out.slice(0, br + 1) + `\n${decls}\n` + out.slice(br + 1);
}

fs.writeFileSync(OUT, out);

console.error(`[way3-setup] ${SRC} -> ${OUT}`);
console.error(`  per-test context: {${Object.keys(perTestCtx).filter(k=>perTestCtx[k]).join(',')||'-'}}   setUp candidates: ${decisions.length}`);
for (const d of decisions) console.error(`    ${d.hoist?'HOIST ':'REFUSE'} $this->${d.prop} = ${d.rhs.slice(0,34)}  (mult=${d.mult})  :: ${d.reason}`);
console.error(`  hoisted: ${hoisted.length}`);
