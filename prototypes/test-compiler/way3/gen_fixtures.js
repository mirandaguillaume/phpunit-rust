#!/usr/bin/env node
// Generates two fixtures that differ ONLY in the SCOPE at which the ambient
// timezone is established:
//   GlobalCtxTest   — date_default_timezone_set('America/Toronto') in setUpBeforeClass (CLASS scope)
//   PerTestCtxTest  — date_default_timezone_set('America/Toronto') in setUp           (PER-TEST scope)
// Both: bootstrap sets UTC as the baseline "system" tz; 20 methods each call the
// shared expensive tz-dependent Heavy::parseExpensive('2024-01-15 10:00:00') and
// assert the Toronto hash. A hoist into setUpBeforeClass is sound ONLY for the
// class-scope variant (Toronto already active there); in the per-test variant the
// hoist slot still sees UTC, so a hoisted value is the WRONG (UTC) hash.
const fs = require('fs');
const N = 20;
const LIT = '2024-01-15 10:00:00';
const TORONTO_HASH = process.argv[2]; // baked correct hash, computed under Toronto
if (!TORONTO_HASH) { console.error('usage: gen_fixtures.js <toronto_hash>'); process.exit(1); }

function methods() {
  let s = '';
  for (let i = 0; i < N; i++) {
    s += `
    public function test${String(i).padStart(2,'0')}(): void
    {
        self::assertSame('${TORONTO_HASH}', \\Cv1\\Heavy::parseExpensive('${LIT}'));
    }
`;
  }
  return s;
}

const globalCtx = `<?php
namespace Cv1\\Tests;

use PHPUnit\\Framework\\TestCase;

/** Timezone established at CLASS scope — stable through the setUpBeforeClass hoist slot. */
final class GlobalCtxTest extends TestCase
{
    public static function setUpBeforeClass(): void
    {
        parent::setUpBeforeClass();
        date_default_timezone_set('America/Toronto');
    }
${methods()}}
`;

const perTestCtx = `<?php
namespace Cv1\\Tests;

use PHPUnit\\Framework\\TestCase;

/** Timezone established at PER-TEST scope — the setUpBeforeClass hoist slot still sees UTC. */
final class PerTestCtxTest extends TestCase
{
    protected function setUp(): void
    {
        parent::setUp();
        date_default_timezone_set('America/Toronto');
    }
${methods()}}
`;

fs.writeFileSync('/tmp/cv1-demo/tests/GlobalCtxTest.php', globalCtx);
fs.writeFileSync('/tmp/cv1-demo/tests/PerTestCtxTest.php', perTestCtx);
console.error(`wrote GlobalCtxTest.php + PerTestCtxTest.php (N=${N}, hash=${TORONTO_HASH})`);
