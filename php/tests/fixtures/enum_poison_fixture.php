<?php

declare(strict_types=1);

namespace PhpunitRust\Tests\Fixtures;

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
