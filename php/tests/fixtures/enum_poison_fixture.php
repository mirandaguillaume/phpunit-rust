<?php

declare(strict_types=1);

namespace PhpunitRust\Tests\Fixtures;

use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\Attributes\RequiresPhpExtension;

/**
 * A class gated by an UNMET requirement, with a heavy (>=15-row) data provider.
 * Its whole suite will be SKIPPED, so the enumerator must return null for the
 * provider (= do not stride-split): a split would emit one skip per chunk and
 * over-count vs vanilla's single skip on PHPUnit >=10.
 */
#[RequiresPhpExtension('phpunit_rust_no_such_ext_xyz')]
final class EnumGated
{
    /** @return list<array{0: int}> */
    public static function rows(): array
    {
        return array_map(static fn (int $i): array => [$i], range(1, 20));
    }
}

/**
 * The method-level residual of EnumGated: NO class requirement, but the test
 * method consuming the heavy provider carries its OWN unmet
 * #[RequiresPhpExtension]. The enumerator can't see the test method from the
 * (class, provider) pair, so it must find the consumer by reflection and return
 * null — otherwise the gated method's provider is stride-split and over-counts
 * by chunks-1 on PHPUnit >=10.
 */
final class EnumMethodGated
{
    /** @return list<array{0: int}> */
    public static function rows(): array
    {
        return array_map(static fn (int $i): array => [$i], range(1, 20));
    }

    #[RequiresPhpExtension('phpunit_rust_no_such_ext_xyz')]
    #[DataProvider('rows')]
    public function testGated(int $i): void {}
}

/**
 * A provider value whose __destruct() throws unless explicitly "consumed" —
 * mirrors php-parser's NodeVisitorForTesting, which asserts in __destruct that
 * its scripted events were all triggered. During row-count enumeration the
 * provider is only counted, never driven, so these objects are NEVER consumed
 * and their destructors throw on teardown.
 */
final class EnumThrowingDtor
{
    public function __construct(private bool $consumed = false) {}

    public function __destruct()
    {
        if (!$this->consumed) {
            throw new \Exception('enum poison: destructor asserts unconsumed');
        }
    }
}

/** Two static providers: one "poison" (throwing destructors), one plain. */
final class EnumPoison
{
    /** @return list<array{0: EnumThrowingDtor}> */
    public static function poison(): array
    {
        return [[new EnumThrowingDtor()], [new EnumThrowingDtor()]];
    }

    /** @return list<array{0: int}> */
    public static function good(): array
    {
        return [[1], [2], [3]];
    }
}
