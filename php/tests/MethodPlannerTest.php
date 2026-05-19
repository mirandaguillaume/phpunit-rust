<?php

declare(strict_types=1);

namespace PhpunitRust\Tests;

use PhpunitRust\MethodPlanner;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\Attributes\Depends;
use PHPUnit\Framework\TestCase;

// Sample fixtures inside the test file (single-file scope).
final class _MpSimple extends TestCase
{
    public function testOne(): void {}
    public function testTwo(): void {}
}

final class _MpProvider extends TestCase
{
    public static function rows(): array
    {
        return ['a' => [1, 2], 'b' => [3, 4]];
    }
    #[DataProvider('rows')]
    public function testParam(int $a, int $b): void {}
}

final class _MpChain extends TestCase
{
    public function testRoot(): int { return 1; }
    #[Depends('testRoot')]
    public function testMiddle(int $r): int { return $r + 1; }
    #[Depends('testMiddle')]
    public function testLeaf(int $r): void {}
}

final class _MpGeneratorMultiSegment extends TestCase
{
    // Regression for the brick/math BigDecimalTest bug: a generator that
    // yields without keys in one segment, then `yield from` an array literal,
    // produces colliding integer keys (each segment restarts at 0). We must
    // append for int keys, not overwrite.
    public static function gen(): \Generator
    {
        yield [1];
        yield [2];
        yield [3];
        yield from [
            [10],
            [20],
        ];
    }
    #[DataProvider('gen')]
    public function testGen(int $x): void {}
}

final class MethodPlannerTest extends TestCase
{
    public function testNonProviderMethodEmitsSingleStep(): void
    {
        $steps = MethodPlanner::plan(_MpSimple::class, ['testOne', 'testTwo']);
        $this->assertCount(2, $steps);
        $this->assertSame('testOne', $steps[0]['method']);
        $this->assertNull($steps[0]['dataset']);
        $this->assertSame([], $steps[0]['args']);
    }

    public function testDataProviderExpandsToOneStepPerRow(): void
    {
        $steps = MethodPlanner::plan(_MpProvider::class, ['testParam']);
        $this->assertCount(2, $steps);
        $this->assertSame('a', $steps[0]['dataset']);
        $this->assertSame([1, 2], $steps[0]['args']);
        $this->assertSame('b', $steps[1]['dataset']);
        $this->assertSame([3, 4], $steps[1]['args']);
    }

    public function testDependsOrdersMethodsTopologically(): void
    {
        // Give in reverse order to prove planner re-orders.
        $steps = MethodPlanner::plan(_MpChain::class, ['testLeaf', 'testMiddle', 'testRoot']);
        $names = array_column($steps, 'method');
        $this->assertSame(['testRoot', 'testMiddle', 'testLeaf'], $names);
    }

    public function testDependsAreReturnedInStep(): void
    {
        $steps = MethodPlanner::plan(_MpChain::class, ['testRoot', 'testMiddle']);
        $this->assertSame([], $steps[0]['depends']);
        $this->assertSame(['testRoot'], $steps[1]['depends']);
    }

    public function testGeneratorWithUnkeyedYieldsAndYieldFromPreservesAllRows(): void
    {
        $steps = MethodPlanner::plan(_MpGeneratorMultiSegment::class, ['testGen']);
        // 3 from the plain yields + 2 from `yield from [...]` = 5 distinct rows.
        // Pre-fix this returned only 3 because keys 0/1/2 from `yield from`
        // overwrote the earlier 3 unkeyed yields.
        $this->assertCount(5, $steps, 'all rows must be preserved across generator segments');
        $args = array_column($steps, 'args');
        $this->assertSame([[1], [2], [3], [10], [20]], $args);
    }
}
