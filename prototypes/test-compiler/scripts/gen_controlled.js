#!/usr/bin/env node
// Generate the CONTROLLED fixture pair: an original where N test methods each rebuild
// the IDENTICAL expensive, deterministic, immutable sub-tree `Heavy::build(1,2,3)`, and
// a COMPILED version where that shared sub-tree is hoisted to a `private static $s0`
// computed ONCE in setUpBeforeClass. Proves the compile->gain mechanism when the shared
// sub-tree is genuinely expensive. `Heavy::build(1,2,3)->read()` is a fixed value
// (EXPECTED), so both versions PASS identically.
const fs = require('fs');
const DIR = process.argv[2] || __dirname + '/../controlled';
const N = 20;
const EXPECTED = 55697; // Heavy::build(1,2,3)->read(), computed once under php8.4.

const header = (cls, doc) =>
`<?php\n\ndeclare(strict_types=1);\n\nuse PHPUnit\\Framework\\TestCase;\n\nrequire_once __DIR__ . '/Heavy.php';\n\n/**\n${doc}\n */\nfinal class ${cls} extends TestCase\n{\n`;

let orig = header('HeavyTest',
` * ORIGINAL: ${N} test methods each independently build the IDENTICAL expensive,\n * deterministic Heavy(1,2,3) and read it. The shared sub-tree is recomputed ${N}x.`);
for (let i = 0; i < N; i++)
  orig += `    public function test${i}(): void\n    {\n        $x = Heavy::build(1, 2, 3);\n        $this->assertSame(${EXPECTED}, $x->read());\n    }\n\n`;
orig += `}\n`;
fs.writeFileSync(DIR + '/HeavyTest.php', orig);

let comp = header('HeavyCompiledTest',
` * COMPILED: the shared deterministic, immutable sub-tree \`Heavy::build(1, 2, 3)\`\n * (e-class multiplicity ${N}) is HOISTED — computed ONCE in setUpBeforeClass into a\n * static memo — and each test references self::$s0 instead of recomputing it.`);
comp += `    private static $s0;\n\n    public static function setUpBeforeClass(): void\n    {\n        parent::setUpBeforeClass();\n        self::$s0 = Heavy::build(1, 2, 3);\n    }\n\n`;
for (let i = 0; i < N; i++)
  comp += `    public function test${i}(): void\n    {\n        $x = self::$s0;\n        $this->assertSame(${EXPECTED}, $x->read());\n    }\n\n`;
comp += `}\n`;
fs.writeFileSync(DIR + '/HeavyCompiledTest.php', comp);

console.error(`[gen_controlled] wrote HeavyTest.php (${N}x build) + HeavyCompiledTest.php (1x build) to ${DIR}`);
