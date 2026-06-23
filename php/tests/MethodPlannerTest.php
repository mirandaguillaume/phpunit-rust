<?php

declare(strict_types=1);

namespace Proust\Tests;

use Proust\MethodPlanner;
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

class _MpThrowingJson implements \JsonSerializable
{
    // Mirrors Carbon\CarbonPeriod (endless): jsonSerialize() throws, so
    // json_encode() of a provider row holding this propagates the throwable.
    public function jsonSerialize(): mixed
    {
        throw new \RuntimeException('cannot be converted to array');
    }
}

final class _MpThrowingProvider extends TestCase
{
    public static function rows(): array
    {
        return [[new _MpThrowingJson()]];
    }
    #[DataProvider('rows')]
    public function testWithThrowingArg(object $o): void {}
}

class _MpPrivateStateObject
{
    // Public state is identical across instances; the distinguishing value is
    // PRIVATE, so json_encode() (which only sees public properties) renders
    // every instance the same. Mirrors php-parser's NodeVisitorForTesting,
    // whose scripted returns live in a private property.
    public array $trace = [];
    public function __construct(private string $secret) {}
}

final class _MpObjectDedupProvider extends TestCase
{
    public static function rows(): array
    {
        // Two DISTINCT objects + an identical scalar tail. Under json_encode
        // both rows render identically, so a naive content hash collides.
        return [
            [new _MpPrivateStateObject('first'), 'same'],
            [new _MpPrivateStateObject('second'), 'same'],
        ];
    }
    #[DataProvider('rows')]
    public function testObj(object $o, string $tag): void {}
}

final class _MpEmptyProvider extends TestCase
{
    // Mirrors faker's localeDataProvider when the locale dirs are absent: the
    // provider returns an empty array.
    public static function none(): array
    {
        return [];
    }
    #[DataProvider('none')]
    public function testNoData(int $x): void {}
}

// A provider living in a DIFFERENT class, referenced from a test method via the
// legacy PHPDoc cross-class form `@dataProvider \FQCN::method`. Vanilla PHPUnit
// splits the annotation on '::' and reflects the external class. The provider
// is intentionally non-test-prefixed so it is not itself collected as a test.
final class _MpExternalProviderSource extends TestCase
{
    public static function rows(): array
    {
        return ['ext_a' => [1, 2], 'ext_b' => [3, 4]];
    }
}

final class _MpLegacyExternalProvider extends TestCase
{
    /**
     * @dataProvider \Proust\Tests\_MpExternalProviderSource::rows
     */
    public function testParam(int $a, int $b): void {}
}

// Two providers on one method that both define the SAME string dataset key.
// Vanilla PHPUnit throws InvalidDataProviderException with the message
// 'The key "%s" has already been defined by a previous data provider'.
final class _MpDuplicateStringKeyProvider extends TestCase
{
    public static function first(): array
    {
        return ['dup' => [1]];
    }
    public static function second(): array
    {
        return ['dup' => [2]];
    }
    #[DataProvider('first')]
    #[DataProvider('second')]
    public function testParam(int $a): void {}
}

// Two providers on one method that both use INTEGER keys. PHP renumbers
// integer keys on append (each provider restarts at 0), so vanilla does NOT
// treat colliding int keys as an error — it appends every row sequentially.
final class _MpDuplicateIntKeyProvider extends TestCase
{
    public static function first(): array
    {
        return [[1], [2]]; // keys 0, 1
    }
    public static function second(): array
    {
        return [[3], [4]]; // keys 0, 1 again — must append, not collide
    }
    #[DataProvider('first')]
    #[DataProvider('second')]
    public function testParam(int $a): void {}
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

    public function testRowWithThrowingJsonSerializeDoesNotCrashPlanning(): void
    {
        // Regression for carbon: a provider row containing an object whose
        // jsonSerialize() throws (Carbon\CarbonPeriod endless) made the dedup
        // hash (json_encode) propagate the throwable and error the whole class.
        // Planning must survive and keep the row (treated as unique, no dedup).
        $steps = MethodPlanner::plan(_MpThrowingProvider::class, ['testWithThrowingArg']);
        $this->assertCount(1, $steps);
        $this->assertSame('testWithThrowingArg', $steps[0]['method']);
    }

    public function testObjectProviderRowsAreNeverDeduplicated(): void
    {
        // Regression for php-parser NodeTraverserTest::testInvalidReturn: two
        // rows carrying DISTINCT objects whose distinguishing state is PRIVATE
        // hash identically under json_encode, so the 2nd row was flagged
        // is_duplicate and memoized — never executed — leaving its object
        // unconsumed. A throwing __destruct on that object then collapsed the
        // whole class. PHPUnit runs every data row, so an object-bearing row
        // must never be deduplicated.
        $steps = MethodPlanner::plan(_MpObjectDedupProvider::class, ['testObj']);
        $this->assertCount(2, $steps);
        $this->assertFalse($steps[0]['is_duplicate'], 'row 0 must not be a duplicate');
        $this->assertFalse(
            $steps[1]['is_duplicate'],
            'object rows must never be deduplicated (private state is invisible to json_encode)'
        );
    }

    public function testEmptyDataProviderStillYieldsOneSkipStep(): void
    {
        // Regression for faker (localeDataProvider empty when no locale dirs):
        // PHPUnit reports a data-provider method that provides NO data as one
        // skipped test, never zero. The planner must emit a single step flagged
        // empty_provider so the method still appears in the count.
        $steps = MethodPlanner::plan(_MpEmptyProvider::class, ['testNoData']);
        $this->assertCount(1, $steps, 'an empty data provider must still yield one step');
        $this->assertSame('testNoData', $steps[0]['method']);
        $this->assertTrue($steps[0]['empty_provider'] ?? false, 'the step must be flagged empty_provider');
    }

    public function testLegacyCrossClassPhpDocProviderExpandsToRows(): void
    {
        // Regression: `@dataProvider \FQCN::method` (the legacy PHPUnit 9 form,
        // still common in PHPUnit 10 codebases) was collapsing the whole method
        // to a single provider_error because the planner reflected the literal
        // token 'Source::rows' on the TEST class. Vanilla splits on '::' and
        // reflects the EXTERNAL class. We must expand to that provider's rows,
        // preserving its dataset keys.
        $steps = MethodPlanner::plan(_MpLegacyExternalProvider::class, ['testParam']);
        $this->assertCount(2, $steps, 'cross-class PHPDoc provider must expand to its rows');
        $this->assertArrayNotHasKey('provider_error', $steps[0]);
        $this->assertSame('ext_a', $steps[0]['dataset']);
        $this->assertSame([1, 2], $steps[0]['args']);
        $this->assertSame('ext_b', $steps[1]['dataset']);
        $this->assertSame([3, 4], $steps[1]['args']);
    }

    public function testDuplicateStringKeyAcrossProvidersEmitsProviderError(): void
    {
        // Vanilla PHPUnit (DataProvider::dataProvidedByMethods) throws
        // InvalidDataProviderException when two providers for the same method
        // define the same string dataset key. The planner must surface this as
        // a single per-method provider_error with the vanilla-matching message,
        // not silently last-wins.
        $steps = MethodPlanner::plan(_MpDuplicateStringKeyProvider::class, ['testParam']);
        $this->assertCount(1, $steps, 'a duplicate key must collapse the method to one error step');
        $this->assertSame('testParam', $steps[0]['method']);
        $this->assertSame(
            'The key "dup" has already been defined by a previous data provider',
            $steps[0]['provider_error'] ?? null,
        );
    }

    public function testDuplicateIntKeysAcrossProvidersAppendRatherThanError(): void
    {
        // PHP arrays renumber integer keys on append, so two providers that both
        // use integer keys 0,1 do NOT collide in vanilla — every row is appended
        // sequentially. The planner must keep all four rows and emit no error.
        $steps = MethodPlanner::plan(_MpDuplicateIntKeyProvider::class, ['testParam']);
        $this->assertCount(4, $steps, 'colliding int keys must append, not error');
        $this->assertArrayNotHasKey('provider_error', $steps[0]);
        $args = array_column($steps, 'args');
        $this->assertSame([[1], [2], [3], [4]], $args);
    }
}
