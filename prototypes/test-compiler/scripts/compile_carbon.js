#!/usr/bin/env node
// Test-compiler prototype — hoisting pass for a PHPUnit test class.
//
// Input : a PHPUnit test file (default: carbon tests/Carbon/DiffTest.php).
// Output: a COMPILED sibling class where each DETERMINISTIC, IMMUTABLE, SHARED
//         sub-tree `Carbon::parse('<literal>')` is computed ONCE in
//         setUpBeforeClass into a `private static $pN`, and every safe occurrence
//         is replaced by `self::$pN`. Non-deterministic `Carbon::now()` is never
//         touched; the rest of the file (namespace, extends, methods, assertions)
//         is byte-identical.
//
// This mirrors what crates/analyzer/src/reduce/egraph.rs already reports: its
// `build_suite_egraph` produces `top_targets` — structurally-shared call-nodes by
// op name (e.g. `Carbon::parse`) with multiplicity m>1. The e-graph proves the
// SHARING; this pass turns a shared sub-tree into a memo and emits valid PHP.
//
// SIDE-CONDITIONS (conservative — when in doubt, do NOT hoist):
//   1. Deterministic   : literal must not be relative (now/today/tomorrow/…); a
//                        Carbon::parse of an ABSOLUTE date string is deterministic.
//                        Carbon::now() is a different op and is never a candidate.
//   2. Immutable use   : a hoisted static is ONE object shared across ALL tests, so
//                        any in-place mutation would corrupt later tests. We hoist an
//                        occurrence ONLY when it is an argument / value position or a
//                        property-read receiver (`->prop`). We EXCLUDE:
//                          - method-call receivers (`parse(...)->m(...)`) — the callee
//                            could mutate ($this->utc() == setTimezone in place);
//                          - assignment RHS (`$x = parse(...);`) — the local alias may
//                            be mutated later (`$x->utc()`), which the identity gate
//                            actually caught on a looser rule.
//   3. Self-contained  : the literal carries no method-local variable, so it is
//                        identical across tests and hoistable into setUpBeforeClass.
//
// Hoist a literal iff it has >= 2 SAFE occurrences (multiplicity > 1).
//
// CORRECTNESS GATE (run after this script): the compiled file MUST produce the same
// PHPUnit result as the original (same tests / assertions / PASS). See
// scripts/run_carbon_compiled.sh.

const fs = require('fs');

const SRC = process.argv[2] ||
  '/tmp/proust-smoke/carbon/tests/Carbon/DiffTest.php';
const OUT = process.argv[3] ||
  '/tmp/proust-smoke/carbon/tests/Carbon/DiffCompiledTest.php';

const s = fs.readFileSync(SRC, 'utf8');
const RELATIVE = /\b(now|today|tomorrow|yesterday|next|last|ago|\+|first day|this )\b/i;

const re = /Carbon::parse\(\s*'([^']*)'\s*\)/g;
let m; const occ = [];
while ((m = re.exec(s)) !== null) {
  const start = m.index, end = m.index + m[0].length, lit = m[1];
  let j = end; while (j < s.length && /\s/.test(s[j])) j++;
  const tail = s.slice(j, j + 40);
  const beforeCtx = s.slice(Math.max(0, start - 40), start);
  const isAssignRHS = /\$[A-Za-z_]\w*\s*=\s*$/.test(beforeCtx) && !/[=!<>]=\s*$/.test(beforeCtx);
  let kind;
  if (/^\??->\s*[A-Za-z_]\w*\s*\(/.test(tail)) kind = 'recv-method'; // EXCLUDE
  else if (/^\??->\s*[A-Za-z_]\w*/.test(tail)) kind = 'recv-prop';   // SAFE
  else if (isAssignRHS) kind = 'assign-rhs';                         // EXCLUDE
  else kind = 'value';                                               // SAFE
  const safe = !RELATIVE.test(lit) && (kind === 'value' || kind === 'recv-prop');
  occ.push({ start, end, lit, kind, safe });
}

const perLit = {};
for (const o of occ) { perLit[o.lit] ??= 0; if (o.safe) perLit[o.lit]++; }
const hoist = Object.keys(perLit).filter(l => perLit[l] >= 2);
hoist.sort((a, b) => perLit[b] - perLit[a] || (a < b ? -1 : 1));
const idx = new Map(hoist.map((l, i) => [l, i]));

const edits = [];
for (const o of occ) {
  if (o.safe && idx.has(o.lit)) edits.push({ start: o.start, end: o.end, text: `self::$p${idx.get(o.lit)}` });
}
edits.sort((a, b) => b.start - a.start);
let out = s;
for (const e of edits) out = out.slice(0, e.start) + e.text + out.slice(e.end);

out = out.replace(/class DiffTest extends/, 'class DiffCompiledTest extends');

const decls = hoist.map((l, i) => `    private static $p${i};`).join('\n');
const assigns = hoist.map((l, i) => `        self::$p${i} = Carbon::parse('${l}');`).join('\n');
const setup =
  `${decls}\n\n` +
  `    public static function setUpBeforeClass(): void\n    {\n` +
  `        parent::setUpBeforeClass();\n${assigns}\n    }\n`;
const co = out.indexOf('class DiffCompiledTest extends');
const br = out.indexOf('{', co);
out = out.slice(0, br + 1) + '\n' + setup + out.slice(br + 1);

fs.writeFileSync(OUT, out);

const refs = (out.match(/self::\$p\d+/g) || []).length;
console.error(`[compile_carbon] ${SRC}`);
console.error(`  occurrences: ${occ.length}  safe: ${occ.filter(o => o.safe).length}  excluded recv-method: ${occ.filter(o => o.kind === 'recv-method').length}  excluded assign-rhs: ${occ.filter(o => o.kind === 'assign-rhs').length}`);
console.error(`  hoisted statics (mult>=2): ${hoist.length}  parse()->self::$pN reads: ${refs - hoist.length}  Carbon::now() untouched: ${(out.match(/Carbon::now\(\)/g) || []).length}`);
console.error(`  wrote ${OUT}`);
