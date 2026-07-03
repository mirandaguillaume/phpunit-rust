<?php

declare(strict_types=1);

namespace Oracle\Tests;

use Oracle\Ops;
use PHPUnit\Framework\TestCase;

final class OpsTest extends TestCase
{
    public function testSum(): void
    {
        // `+` -> `-` makes this 3 -> -1: the Plus mutant is KILLED.
        self::assertSame(3, (new Ops())->sum(1, 2));
    }

    public function testGteWhenStrictlyGreater(): void
    {
        // Only the strictly-greater case is exercised, never the equal boundary,
        // so `>=` -> `>` still returns true here: the mutant ESCAPES (both tools).
        self::assertTrue((new Ops())->gte(5, 3));
    }

    public function testBand(): void
    {
        // 6 & 3 = 2; mutated 6 | 3 = 7 -> KILLED.
        self::assertSame(2, (new Ops())->band(6, 3));
    }

    public function testBor(): void
    {
        // 6 | 3 = 7; mutated 6 & 3 = 2 -> KILLED.
        self::assertSame(7, (new Ops())->bor(6, 3));
    }

    public function testBxor(): void
    {
        // 6 ^ 3 = 5; mutated 6 & 3 = 2 -> KILLED.
        self::assertSame(5, (new Ops())->bxor(6, 3));
    }

    public function testShl(): void
    {
        // 1 << 3 = 8; mutated 1 >> 3 = 0 -> KILLED.
        self::assertSame(8, (new Ops())->shl(1, 3));
    }

    public function testShr(): void
    {
        // 8 >> 2 = 2; mutated 8 << 2 = 32 -> KILLED.
        self::assertSame(2, (new Ops())->shr(8, 2));
    }

    // Cast mutants UNWRAP the cast; each assertion is strict, so the unwrapped value
    // (different type) fails -> KILLED.
    public function testCi(): void
    {
        self::assertSame(5, (new Ops())->ci('5'));
    }

    public function testCf(): void
    {
        self::assertSame(1.5, (new Ops())->cf('1.5'));
    }

    public function testCs(): void
    {
        self::assertSame('5', (new Ops())->cs(5));
    }

    public function testCb(): void
    {
        self::assertSame(true, (new Ops())->cb(1));
    }

    public function testCa(): void
    {
        self::assertSame([5], (new Ops())->ca(5));
    }

    public function testCo(): void
    {
        self::assertInstanceOf(\stdClass::class, (new Ops())->co(['x' => 1]));
    }

    public function testExpo(): void
    {
        // 2 ** 3 = 8; mutated 2 / 3 = 0 -> KILLED.
        self::assertSame(8, (new Ops())->expo(2, 3));
    }

    public function testPreinc(): void
    {
        // ++$n on 5 = 6; mutated --$n = 4 -> KILLED (Increment).
        self::assertSame(6, (new Ops())->preinc(5));
    }

    public function testPredec(): void
    {
        // --$n on 5 = 4; mutated ++$n = 6 -> KILLED (Decrement).
        self::assertSame(4, (new Ops())->predec(5));
    }

    public function testFive(): void
    {
        // literal 5; IncrementInteger -> 6, DecrementInteger -> 4, both -> KILLED.
        self::assertSame(5, (new Ops())->five());
    }

    // Each comparison has TWO mutants (boundary shift + negation flip). Two assertions
    // — one at the boundary, one strict — kill both.
    public function testGt(): void
    {
        $o = new Ops();
        self::assertTrue($o->gt(5, 3));   // kills GreaterThanNegotiation (`<=`)
        self::assertFalse($o->gt(5, 5));  // kills GreaterThan (`>=`)
    }

    public function testLt(): void
    {
        $o = new Ops();
        self::assertTrue($o->lt(3, 5));
        self::assertFalse($o->lt(5, 5));
    }

    public function testLte(): void
    {
        $o = new Ops();
        self::assertTrue($o->lte(5, 5));   // kills both (`<` and `>` both false at equality)
        self::assertFalse($o->lte(6, 5));
    }

    // Equality has a flip mutant AND a loosen/tighten mutant; a type-juggling case
    // (1 == "1" but 1 !== "1") kills both at once.
    public function testEq(): void
    {
        self::assertTrue((new Ops())->eq(1, '1'));
    }

    public function testNeq(): void
    {
        self::assertFalse((new Ops())->neq(1, '1'));
    }

    public function testIdn(): void
    {
        self::assertFalse((new Ops())->idn(1, '1'));
    }

    public function testNidn(): void
    {
        self::assertTrue((new Ops())->nidn(1, '1'));
    }

    public function testNot(): void
    {
        // !$x on true = false; mutated (unwrapped) $x = true -> KILLED (LogicalNot).
        self::assertFalse((new Ops())->not(true));
    }

    public function testOne(): void
    {
        // 1.0 -> 0.0 -> KILLED (OneZeroFloat).
        self::assertSame(1.0, (new Ops())->one());
    }

    // Compound assignments: swap the arithmetic half -> a different result -> KILLED.
    public function testPe(): void
    {
        self::assertSame(8, (new Ops())->pe(5, 3));
    }

    public function testMe(): void
    {
        self::assertSame(2, (new Ops())->me(5, 3));
    }

    public function testMule(): void
    {
        self::assertSame(15, (new Ops())->mule(5, 3));
    }

    public function testDive(): void
    {
        self::assertSame(2, (new Ops())->dive(6, 3));
    }

    public function testMode(): void
    {
        self::assertSame(1, (new Ops())->mode(7, 3));
    }

    public function testPowe(): void
    {
        self::assertSame(8, (new Ops())->powe(2, 3));
    }

    // Unwrap mutants replace the call with its first argument -> a different result.
    public function testLow(): void
    {
        self::assertSame('abc', (new Ops())->low('ABC'));
    }

    public function testUp(): void
    {
        self::assertSame('ABC', (new Ops())->up('abc'));
    }

    public function testTr(): void
    {
        self::assertSame('x', (new Ops())->tr('  x  '));
    }

    public function testUf(): void
    {
        self::assertSame('Abc', (new Ops())->uf('abc'));
    }

    public function testRev(): void
    {
        self::assertSame('cba', (new Ops())->rev('abc'));
    }

    public function testArev(): void
    {
        self::assertSame([3, 2, 1], (new Ops())->arev([1, 2, 3]));
    }

    public function testAuniq(): void
    {
        self::assertSame([1], (new Ops())->auniq([1, 1]));
    }

    public function testAvals(): void
    {
        self::assertSame([1], (new Ops())->avals(['a' => 1]));
    }

    public function testAflip(): void
    {
        self::assertSame(['b' => 'a'], (new Ops())->aflip(['a' => 'b']));
    }

    public function testSelf(): void
    {
        // return $this -> return null (This) -> KILLED.
        self::assertInstanceOf(Ops::class, (new Ops())->self_());
    }

    public function testShip(): void
    {
        // 1 <=> 2 = -1; swapped 2 <=> 1 = 1 -> KILLED (Spaceship).
        self::assertSame(-1, (new Ops())->ship(1, 2));
    }

    public function testCoal(): void
    {
        // 3 ?? 5 = 3; swapped 5 ?? 3 = 5 -> KILLED (Coalesce).
        self::assertSame(3, (new Ops())->coal(3, 5));
    }

    public function testTern(): void
    {
        // $a ? 'y' : 'n' swapped -> $a ? 'n' : 'y' -> KILLED (Ternary).
        self::assertSame('y', (new Ops())->tern(true));
    }

    public function testCat(): void
    {
        // 'a' . 'b' = 'ab'; swapped 'b' . 'a' = 'ba' -> KILLED (Concat).
        self::assertSame('ab', (new Ops())->cat('a', 'b'));
    }

    public function testBnot(): void
    {
        // ~0 = -1; unwrapped $x = 0 -> KILLED (BitwiseNot).
        self::assertSame(-1, (new Ops())->bnot(0));
    }

    public function testRun(): void
    {
        // removing `$this->record('x');` leaves the log empty -> KILLED (MethodCallRemoval).
        self::assertSame(['x'], (new Ops())->run());
    }

    public function testRunF(): void
    {
        // removing `array_push($out, $v);` leaves $out empty -> KILLED (FunctionCallRemoval).
        $out = [];
        (new Ops())->runF($out, 'z');
        self::assertSame(['z'], $out);
    }

    public function testPick(): void
    {
        // break after first -> 'a'; mutated to continue -> last item 'b' -> KILLED (Break_).
        self::assertSame('a', (new Ops())->pick(['a', 'b']));
    }

    public function testSkipFirst(): void
    {
        // continue skips the first -> ['b','c']; mutated to break -> [] -> KILLED (Continue_).
        self::assertSame(['b', 'c'], (new Ops())->skipFirst(['a', 'b', 'c']));
    }

    public function testBoom(): void
    {
        // removing `throw` -> no exception -> KILLED (Throw_).
        $this->expectException(\RuntimeException::class);
        (new Ops())->boom();
    }

    public function testIsRuntime(): void
    {
        // instanceof -> true kills the `false` mutant; -> false kills the `true` mutant.
        self::assertTrue((new Ops())->isRuntime(new \RuntimeException()));
        self::assertFalse((new Ops())->isRuntime(new \stdClass()));
    }
}
