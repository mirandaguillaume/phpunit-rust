#!/usr/bin/env node
// Test-compiler prototype — ONE-SHOT + CLONE variant (refinement 1).
//
// Carbon instances are MUTABLE, so a hoisted shared instance cannot be passed to a
// test that mutates it (`->addDay()`, `->utc()`, an aliased `$x = ...; $x->setX()`).
// But we can still pay the construction cost ONCE and hand each use an isolated copy:
//
//   private static $proto0;
//   public static function setUpBeforeClass(): void { ...; self::$proto0 = Carbon::parse('lit'); }
//   ... (clone self::$proto0) ...     // per-use fresh object; isolation preserved
//
// `clone` of a warm Carbon is ~0.15 us vs ~1 us for parse (warm) — ~85% cheaper — so the
// saving is (cost(parse) - cost(clone)) x (mult - 1). This lets the MUTATED occurrences
// (method-call receivers + assignment-RHS aliases) that the read-only compiler had to
// EXCLUDE also participate, because each `clone` is independent and cannot leak mutation.
//
// Still gated: the compiled file must give IDENTICAL PHPUnit results as the original.
//
// Determinism rule unchanged: only ABSOLUTE-date `Carbon::parse('<lit>')` literals;
// `Carbon::now()` is never a candidate (see README — DiffTest freezes now() to a MOVING
// per-test target via setUp/wrapWithTestNow, so now() is NOT a fixed deterministic value).

const fs = require('fs');
const SRC = process.argv[2] || '/tmp/proust-smoke/carbon/tests/Carbon/DiffTest.php';
const OUT = process.argv[3] || '/tmp/proust-smoke/carbon/tests/Carbon/DiffCloneTest.php';

const s = fs.readFileSync(SRC, 'utf8');
const RELATIVE = /\b(now|today|tomorrow|yesterday|next|last|ago|\+|first day|this )\b/i;

const re = /Carbon::parse\(\s*'([^']*)'\s*\)/g;
let m; const occ = [];
while ((m = re.exec(s)) !== null) {
  const start = m.index, end = m.index + m[0].length, lit = m[1];
  occ.push({ start, end, lit, det: !RELATIVE.test(lit) });
}
// Every DETERMINISTIC occurrence is clone-eligible (isolation via clone). Hoist a literal
// iff it has >= 2 deterministic occurrences.
const perLit = {};
for (const o of occ) { perLit[o.lit] ??= 0; if (o.det) perLit[o.lit]++; }
const hoist = Object.keys(perLit).filter(l => perLit[l] >= 2);
hoist.sort((a, b) => perLit[b] - perLit[a] || (a < b ? -1 : 1));
const idx = new Map(hoist.map((l, i) => [l, i]));

const edits = [];
for (const o of occ) {
  if (o.det && idx.has(o.lit)) edits.push({ start: o.start, end: o.end, text: `(clone self::$proto${idx.get(o.lit)})` });
}
edits.sort((a, b) => b.start - a.start);
let out = s;
for (const e of edits) out = out.slice(0, e.start) + e.text + out.slice(e.end);

out = out.replace(/class DiffTest extends/, 'class DiffCloneTest extends');

const decls = hoist.map((l, i) => `    private static $proto${i};`).join('\n');
const assigns = hoist.map((l, i) => `        self::$proto${i} = Carbon::parse('${l}');`).join('\n');
const setup =
  `${decls}\n\n    public static function setUpBeforeClass(): void\n    {\n` +
  `        parent::setUpBeforeClass();\n${assigns}\n    }\n`;
const co = out.indexOf('class DiffCloneTest extends');
const br = out.indexOf('{', co);
out = out.slice(0, br + 1) + '\n' + setup + out.slice(br + 1);

fs.writeFileSync(OUT, out);
const refs = (out.match(/clone self::\$proto\d+/g) || []).length;
console.error(`[compile_carbon_clone] hoisted protos: ${hoist.length}  parse->(clone): ${refs}  (vs read-only variant's 84)  now() untouched: ${(out.match(/Carbon::now\(\)/g) || []).length}`);
console.error(`  wrote ${OUT}`);
