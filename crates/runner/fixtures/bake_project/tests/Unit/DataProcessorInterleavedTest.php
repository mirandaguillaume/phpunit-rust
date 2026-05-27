<?php

declare(strict_types=1);

namespace Bake\Tests\Unit;

use Bake\Contract\LoggerInterface;
use Bake\Contract\RepositoryInterface;
use Bake\Service\DataProcessor;
use PHPUnit\Framework\TestCase;

/**
 * Tests with interleaved mock expectations and variables defined after createMock.
 */
class DataProcessorInterleavedTest extends TestCase
{
    public function testInterleavedExpectations(): void
    {
        $repo = $this->createMock(RepositoryInterface::class);
        $logger = $this->createMock(LoggerInterface::class);

        // Variables defined AFTER both createMock calls — the core scoping issue.
        $item = ['id' => 7, 'name' => 'interleaved'];
        $message = 'done';

        // Interleaved: $repo then $logger then $repo again.
        $repo->method('find')->willReturn($item);
        $logger->method('getLastMessage')->willReturn($message);
        $repo->method('findAll')->willReturn([$item]);

        $processor = new DataProcessor($repo, $logger);

        $this->assertEquals($item, $processor->process(7));
        $this->assertEquals($message, $processor->getLastLog());
        $this->assertCount(1, $processor->processAll());
    }

    public function testMultipleExpectationsOnSameMock(): void
    {
        $repo = $this->createMock(RepositoryInterface::class);
        $logger = $this->createMock(LoggerInterface::class);

        $itemA = ['id' => 1];
        $itemB = ['id' => 2];

        $repo->method('find')->willReturn($itemA);
        $repo->method('save')->willReturn(false);
        $logger->method('log');
        $logger->method('getLastMessage')->willReturn('ok');
        $logger->method('getCode')->willReturn(0);

        $processor = new DataProcessor($repo, $logger);

        $this->assertEquals($itemA, $processor->process(1));
        $this->assertFalse($processor->save($itemB));
        $this->assertEquals('ok', $processor->getLastLog());
        $this->assertEquals(0, $processor->getLoggerCode());
    }

    public function testWillReturnLiteralValue(): void
    {
        $repo = $this->createMock(RepositoryInterface::class);
        $logger = $this->createMock(LoggerInterface::class);

        // Literals directly in willReturn (no outer variable).
        $repo->method('count')->willReturn(99);
        $logger->method('getCode')->willReturn(7);

        $processor = new DataProcessor($repo, $logger);

        $this->assertEquals(7, $processor->getLoggerCode());
    }
}
