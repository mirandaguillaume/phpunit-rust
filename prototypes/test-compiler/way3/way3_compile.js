#!/usr/bin/env node
// Way-3 compiler — CONTEXTUAL-DETERMINISM hoisting.
//
// Hoists a shared sub-tree into setUpBeforeClass ONLY when every ambient context
// it could read (timezone / frozen-now / locale) is PROVABLY THE SAME at the
// hoist slot (setUpBeforeClass, after class-scope setters) as at the use site
// (test body). The proof is by SCOPE: a context setter in setUpBeforeClass /
// bootstrap / a parent's setUpBeforeClass is CLASS/GLOBAL scope (stable through
// the hoist slot); a setter in setUp / a parent's setUp is PER-TEST scope (the
// hoist slot runs BEFORE it, so the context differs).
//
// Rule (v1, conservative on opaque calls): a candidate call is opaque, so it
// could read any ambient context. Therefore HOIST iff NO ambient context is
// (re)established at per-test scope; otherwise REFUSE. This statically refuses
// exactly the hoists the clone-refinement's runtime identity gate caught as
// failures (carbon: tz set per-test in AbstractTestCase::setUp).
//
// usage: way3_compile.js <src> <out> [parentFile ...]
const fs = require('fs');

const NAIVE = process.argv.includes('--naive');
const positional = process.argv.slice(2).filter(a => a !== '--naive');
const SRC = positional[0];
const OUT = positional[1];
const PARENTS = positional.slice(2);
if (!SRC || !OUT) { console.error('usage: way3_compile.js [--naive] <src> <out> [parent ...]'); process.exit(1); }

const s = fs.readFileSync(SRC, 'utf8');

// ---- brace matching -------------------------------------------------------
function matchBrace(str, open) {
  let d = 0;
  for (let i = open; i < str.length; i++) {
    const c = str[i];
    if (c === '{') d++;
    else if (c === '}') { d--; if (d === 0) return i; }
  }
  return -1;
}
// find a method body by name; returns {open, close, body} or null
function methodBody(str, name) {
  const re = new RegExp(`function\\s+${name}\\s*\\(`, 'g');
  const m = re.exec(str);
  if (!m) return null;
  const open = str.indexOf('{', m.index);
  if (open < 0) return null;
  const close = matchBrace(str, open);
  return { sig: m.index, open, close, body: str.slice(open + 1, close) };
}

// ---- context-scope analysis ----------------------------------------------
const SETTERS = {
  tz:     /\bdate_default_timezone_set\s*\(/,
  now:    /\b(setTestNow|setTestNowAndTimezone)\s*\(/,
  locale: /\bsetlocale\s*\(/,
};
function settersIn(text) {
  const out = {};
  for (const k of Object.keys(SETTERS)) out[k] = !!text && SETTERS[k].test(text);
  return out;
}
function orScope(a, b) {
  return { tz: a.tz || b.tz, now: a.now || b.now, locale: a.locale || b.locale };
}

// gather setter scopes from this file + parents
let classScope  = { tz: false, now: false, locale: false };
let perTestScope = { tz: false, now: false, locale: false };

function accumulate(text) {
  const sub = methodBody(text, 'setUpBeforeClass');
  const su  = methodBody(text, 'setUp'); // NB: 'setUp' regex would also match setUpBeforeClass; guard below
  if (sub) classScope = orScope(classScope, settersIn(sub.body));
  // setUp (the per-test one) — find a setUp that is NOT setUpBeforeClass
  const reSetUp = /function\s+setUp\s*\(/g; let mm;
  while ((mm = reSetUp.exec(text)) !== null) {
    // ensure the char before is not part of "setUpBeforeClass"
    const after = text.slice(mm.index, mm.index + 30);
    if (/function\s+setUp\s*\(/.test(after) && !/setUpBeforeClass/.test(after)) {
      const open = text.indexOf('{', mm.index);
      const close = matchBrace(text, open);
      perTestScope = orScope(perTestScope, settersIn(text.slice(open + 1, close)));
    }
  }
}
accumulate(s);
for (const p of PARENTS) { if (fs.existsSync(p)) accumulate(fs.readFileSync(p, 'utf8')); }

const anyPerTest = perTestScope.tz || perTestScope.now || perTestScope.locale;

// ---- candidate shared sub-trees ------------------------------------------
// Call with a single string-literal arg: Class::method('literal') (handles \Cv1\Heavy too).
const reCall = /(\\?[A-Za-z_][\w\\]*::[A-Za-z_]\w*)\(\s*'([^']*)'\s*\)/g;
let m; const occ = [];
while ((m = reCall.exec(s)) !== null) {
  occ.push({ start: m.index, end: m.index + m[0].length, text: m[0], op: m[1], lit: m[2] });
}
const byText = {};
for (const o of occ) (byText[o.text] ??= []).push(o);

// decide per distinct candidate
const decisions = [];
for (const [text, list] of Object.entries(byText)) {
  const mult = list.length;
  if (mult < 2) continue;
  // hoist iff no per-test ambient context (conservative; opaque call).
  // --naive ignores context entirely (the unsound baseline, for contrast).
  const hoist = NAIVE ? true : !anyPerTest;
  const reason = NAIVE
    ? 'NAIVE: hoisted without any context check'
    : hoist
    ? (classScope.tz || classScope.now || classScope.locale
        ? 'ambient context established at CLASS scope — stable through hoist slot'
        : 'no ambient context dependency in suite lifecycle')
    : `ambient context re-established PER-TEST (${Object.keys(perTestScope).filter(k=>perTestScope[k]).join(',')}) — hoist slot sees a different context`;
  decisions.push({ text, op: list[0].op, lit: list[0].lit, mult, hoist, reason });
}
decisions.sort((a, b) => b.mult - a.mult);

const hoisted = decisions.filter(d => d.hoist);
const idx = new Map(hoisted.map((d, i) => [d.text, i]));

// ---- emit -----------------------------------------------------------------
const origClass = (s.match(/class\s+(\w+)\s+extends/) || [])[1];
const newClass = origClass ? origClass.replace(/Test$/, '') + 'CompiledTest' : 'CompiledTest';

// replace hoisted occurrences with self::$hN (right-to-left)
const edits = [];
for (const o of occ) if (idx.has(o.text)) edits.push({ start: o.start, end: o.end, repl: `self::$h${idx.get(o.text)}` });
edits.sort((a, b) => b.start - a.start);
let out = s;
for (const e of edits) out = out.slice(0, e.start) + e.repl + out.slice(e.end);

// rename class
out = out.replace(new RegExp(`class\\s+${origClass}\\b`), `class ${newClass}`);

if (hoisted.length) {
  const decls = hoisted.map((d, i) => `    private static $h${i};`).join('\n');
  const assigns = hoisted.map((d, i) => `        self::$h${i} = ${d.text};`).join('\n');
  // insert assignments at END of setUpBeforeClass body (AFTER class-scope setters),
  // creating the method if absent.
  const sub = methodBody(out, 'setUpBeforeClass');
  if (sub) {
    out = out.slice(0, sub.close) + assigns + '\n    ' + out.slice(sub.close);
  } else {
    const co = out.indexOf(`class ${newClass}`);
    const br = out.indexOf('{', co);
    const method =
      `\n    public static function setUpBeforeClass(): void\n    {\n` +
      `        parent::setUpBeforeClass();\n${assigns}\n    }\n`;
    out = out.slice(0, br + 1) + method + out.slice(br + 1);
  }
  // declarations right after the class opening brace
  const co = out.indexOf(`class ${newClass}`);
  const br = out.indexOf('{', co);
  out = out.slice(0, br + 1) + `\n${decls}\n` + out.slice(br + 1);
}

fs.writeFileSync(OUT, out);

// ---- report ---------------------------------------------------------------
console.error(`[way3] ${SRC} -> ${OUT}`);
console.error(`  context scopes: class={${Object.keys(classScope).filter(k=>classScope[k]).join(',')||'-'}} perTest={${Object.keys(perTestScope).filter(k=>perTestScope[k]).join(',')||'-'}}`);
console.error(`  distinct shared candidates (mult>=2): ${decisions.length}`);
for (const d of decisions) {
  console.error(`    ${d.hoist ? 'HOIST ' : 'REFUSE'} mult=${d.mult}  ${d.op}('${d.lit.slice(0,32)}')  :: ${d.reason}`);
}
console.error(`  hoisted statics: ${hoisted.length}  occurrences replaced: ${edits.length}`);
