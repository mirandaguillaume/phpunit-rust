<?php

declare(strict_types=1);

namespace PhpunitRust\Attributes;

use Attribute;

/**
 * Opt-in marker: this test class (or method) requires a provisioned database
 * clone. The phpunit-rust runner provisions a per-worker-slot database and
 * wraps each test in a transaction that is rolled back, so writes never leak
 * across tests or workers. Detected statically at discovery time.
 */
#[Attribute(Attribute::TARGET_CLASS | Attribute::TARGET_METHOD)]
final class UsesDatabase
{
}
