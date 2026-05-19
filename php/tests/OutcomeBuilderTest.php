<?php

declare(strict_types=1);

namespace PhpunitRust\Tests;

use PhpunitRust\OutcomeBuilder;
use PHPUnit\Framework\AssertionFailedError;
use PHPUnit\Framework\ExpectationFailedException;
use PHPUnit\Framework\IncompleteTestError;
use PHPUnit\Framework\SkippedWithMessageException;
use PHPUnit\Framework\TestCase;

final class OutcomeBuilderTest extends TestCase
{
    public function testPassNoException(): void
    {
        $outcome = OutcomeBuilder::build('A', 'm', null, 1.5, null);
        $this->assertSame('pass', $outcome['status']);
        $this->assertNull($outcome['message']);
        $this->assertSame(1.5, $outcome['duration_ms']);
    }

    public function testFailFromExpectationFailed(): void
    {
        $e = new ExpectationFailedException('boom');
        $outcome = OutcomeBuilder::build('A', 'm', null, 0.1, $e);
        $this->assertSame('fail', $outcome['status']);
        $this->assertSame('boom', $outcome['message']);
        $this->assertNotNull($outcome['trace']);
    }

    public function testFailFromAssertionFailed(): void
    {
        $e = new AssertionFailedError('nope');
        $outcome = OutcomeBuilder::build('A', 'm', null, 0.1, $e);
        $this->assertSame('fail', $outcome['status']);
    }

    public function testSkipped(): void
    {
        $e = new SkippedWithMessageException('because reasons');
        $outcome = OutcomeBuilder::build('A', 'm', null, 0.1, $e);
        $this->assertSame('skipped', $outcome['status']);
        $this->assertSame('because reasons', $outcome['message']);
    }

    public function testIncomplete(): void
    {
        $e = new IncompleteTestError('todo');
        $outcome = OutcomeBuilder::build('A', 'm', null, 0.1, $e);
        $this->assertSame('incomplete', $outcome['status']);
    }

    public function testGenericThrowableBecomesError(): void
    {
        $e = new \RuntimeException('crash');
        $outcome = OutcomeBuilder::build('A', 'm', null, 0.1, $e);
        $this->assertSame('error', $outcome['status']);
        $this->assertStringContainsString('RuntimeException', $outcome['message']);
    }

    public function testDatasetIsPropagated(): void
    {
        $outcome = OutcomeBuilder::build('A', 'm', 'with foo', 0.1, null);
        $this->assertSame('with foo', $outcome['dataset']);
    }
}
