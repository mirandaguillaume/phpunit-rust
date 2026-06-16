#!/usr/bin/env node
// Way-3 OFFER scanner — static, no execution.
// For each suite: resolve each concrete test class's inheritance chain, classify
// ambient-context setters by SCOPE (class/global vs per-test), find shared call
// sub-trees (mult>=2) per class, and decide HOIST vs REFUSE per Way-3 v1.
// Aggregates per suite so we can see WHERE the hoistable shape actually exists.
//
// usage: way3_scan.js <suiteRoot> [<suiteRoot> ...]
const fs = require('fs');
const path = require('path');

const SETTERS = {
  tz:     /\bdate_default_timezone_set\s*\(/,
  now:    /\b(setTestNow|setTestNowAndTimezone)\s*\(/,
  locale: /\bsetlocale\s*\(/,
};
const reCall = /(\\?[A-Za-z_][\w\\]*::[A-Za-z_]\w*)\(\s*'([^']*)'\s*\)/g;

function walk(dir, acc) {
  let ents; try { ents = fs.readdirSync(dir, { withFileTypes: true }); } catch { return acc; }
  for (const e of ents) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) { if (!/\/(vendor|node_modules)\b/.test(p)) walk(p, acc); }
    else if (e.name.endsWith('.php')) acc.push(p);
  }
  return acc;
}
function matchBrace(str, open) {
  let d = 0;
  for (let i = open; i < str.length; i++) { const c = str[i]; if (c === '{') d++; else if (c === '}') { d--; if (d === 0) return i; } }
  return -1;
}
function methodBodies(text, name) {
  const bodies = [];
  const re = new RegExp(`function\\s+${name}\\s*\\(`, 'g'); let m;
  while ((m = re.exec(text)) !== null) {
    const after = text.slice(m.index, m.index + 30);
    if (name === 'setUp' && /setUpBeforeClass/.test(after)) continue;
    const open = text.indexOf('{', m.index); if (open < 0) continue;
    const close = matchBrace(text, open); if (close < 0) continue;
    bodies.push(text.slice(open + 1, close));
  }
  return bodies;
}
function settersIn(text) { const o = {}; for (const k in SETTERS) o[k] = !!text && SETTERS[k].test(text); return o; }
function orS(a, b) { return { tz: a.tz||b.tz, now: a.now||b.now, locale: a.locale||b.locale }; }
const EMPTY = { tz:false, now:false, locale:false };
const any = s => s.tz || s.now || s.locale;
const keys = s => Object.keys(s).filter(k => s[k]).join(',') || '-';

function scanSuite(root) {
  const files = walk(path.join(root, 'tests'), []);
  if (!files.length) walk(root, files); // fallback: whole suite
  // class index
  const idx = {}; // className -> { file, text, extends }
  for (const f of files) {
    let text; try { text = fs.readFileSync(f, 'utf8'); } catch { continue; }
    const re = /\b(?:abstract\s+|final\s+)?class\s+(\w+)(?:\s+extends\s+(\\?[\w\\]+))?/g; let m;
    while ((m = re.exec(text)) !== null) idx[m[1]] = { file: f, text, ext: m[2] ? m[2].replace(/^.*\\/, '') : null };
  }
  // per-test-scope context, resolving chain
  function perTestScope(cls, seen = new Set()) {
    if (!cls || seen.has(cls) || !idx[cls]) return EMPTY; seen.add(cls);
    let s = EMPTY;
    for (const b of methodBodies(idx[cls].text, 'setUp')) s = orS(s, settersIn(b));
    return orS(s, perTestScope(idx[cls].ext, seen));
  }
  function classScope(cls, seen = new Set()) {
    if (!cls || seen.has(cls) || !idx[cls]) return EMPTY; seen.add(cls);
    let s = EMPTY;
    for (const b of methodBodies(idx[cls].text, 'setUpBeforeClass')) s = orS(s, settersIn(b));
    return orS(s, classScope(idx[cls].ext, seen));
  }

  let nClasses = 0, nWithPerTest = 0, nHoistClasses = 0;
  let candDistinct = 0, candHoist = 0, candRefuse = 0, occHoist = 0, maxMultHoist = 0;
  const scopeAgg = { tz:false, now:false, locale:false };
  let perTestScopeAgg = { tz:false, now:false, locale:false };

  for (const [cls, info] of Object.entries(idx)) {
    if (!/function\s+test/i.test(info.text) && !/#\[Test\]/.test(info.text)) continue; // concrete test class
    nClasses++;
    const pt = perTestScope(cls); const cs = classScope(cls);
    perTestScopeAgg = orS(perTestScopeAgg, pt); Object.assign(scopeAgg, orS(scopeAgg, cs));
    const refuse = any(pt);
    if (refuse) nWithPerTest++;
    // shared candidates within this class file
    const occ = {}; let m;
    const re = new RegExp(reCall.source, 'g');
    while ((m = re.exec(info.text)) !== null) (occ[m[0]] ??= []).push(m[1]);
    let classHoisted = 0;
    for (const [txt, list] of Object.entries(occ)) {
      if (list.length < 2) continue;
      candDistinct++;
      if (refuse) { candRefuse++; }
      else { candHoist++; occHoist += list.length; classHoisted++; maxMultHoist = Math.max(maxMultHoist, list.length); }
    }
    if (!refuse && classHoisted > 0) nHoistClasses++;
  }
  return {
    suite: path.basename(root), nClasses, nWithPerTest, nHoistClasses,
    candDistinct, candHoist, candRefuse, occHoist, maxMultHoist,
    perTest: keys(perTestScopeAgg), classScope: keys(scopeAgg),
  };
}

const roots = process.argv.slice(2);
const rows = roots.map(scanSuite);
// print table
const H = ['suite','testCls','perTestCtx','hoistCls','sharedDistinct','HOIST','REFUSE','occHoist','maxMult','perTestScopes'];
const fmt = r => [r.suite, r.nClasses, r.nWithPerTest, r.nHoistClasses, r.candDistinct, r.candHoist, r.candRefuse, r.occHoist, r.maxMultHoist, r.perTest];
const widths = H.map((h, i) => Math.max(h.length, ...rows.map(r => String(fmt(r)[i]).length)));
const line = cols => cols.map((c, i) => String(c).padEnd(widths[i])).join('  ');
console.log(line(H));
console.log(widths.map(w => '-'.repeat(w)).join('  '));
for (const r of rows) console.log(line(fmt(r)));
// totals
const T = rows.reduce((a, r) => ({ c:a.c+r.candDistinct, h:a.h+r.candHoist, rf:a.rf+r.candRefuse, hc:a.hc+r.nHoistClasses, tc:a.tc+r.nClasses }), {c:0,h:0,rf:0,hc:0,tc:0});
console.log('\nTOTAL  testClasses=%d  hoistClasses=%d  sharedDistinct=%d  HOIST=%d  REFUSE=%d', T.tc, T.hc, T.c, T.h, T.rf);
