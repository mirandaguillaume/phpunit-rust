#!/usr/bin/env node
// Two fixtures sharing an expensive deterministic setUp (HeavySchema::compile()):
//   SchemaReadTest   — tests only READ the schema  -> hoist setUp->setUpBeforeClass is SOUND
//   SchemaMutateTest — tests MUTATE via addTable()  -> hoisting would corrupt later tests
const fs = require('fs');
const N = 20;
const FP = process.argv[2];
if (!FP) { console.error('usage: gen_schema_fixtures.js <fingerprint>'); process.exit(1); }

function readMethods() {
  let s = '';
  for (let i = 0; i < N; i++) s += `
    public function test${String(i).padStart(2,'0')}(): void
    {
        self::assertSame('${FP}', $this->schema->fingerprint());
        self::assertSame(3, $this->schema->tableCount());
    }
`;
  return s;
}
function mutateMethods() {
  // each test adds ONE table to its (supposedly fresh) schema and asserts count==4.
  // sound only if setUp rebuilds per test; if the schema is shared, counts accumulate.
  let s = '';
  for (let i = 0; i < N; i++) s += `
    public function test${String(i).padStart(2,'0')}(): void
    {
        $this->schema->addTable('extra${i}');
        self::assertSame(4, $this->schema->tableCount());
    }
`;
  return s;
}
const head = (cls) => `<?php
namespace Cv1\\Tests;

use PHPUnit\\Framework\\TestCase;
use Cv1\\HeavySchema;

final class ${cls} extends TestCase
{
    private HeavySchema $schema;

    protected function setUp(): void
    {
        $this->schema = HeavySchema::compile();
    }
`;
fs.writeFileSync('/tmp/cv1-demo/tests/SchemaReadTest.php',   head('SchemaReadTest')   + readMethods()   + '}\n');
fs.writeFileSync('/tmp/cv1-demo/tests/SchemaMutateTest.php', head('SchemaMutateTest') + mutateMethods() + '}\n');
console.error(`wrote SchemaReadTest.php (read-only) + SchemaMutateTest.php (mutating), N=${N}, fp=${FP}`);
