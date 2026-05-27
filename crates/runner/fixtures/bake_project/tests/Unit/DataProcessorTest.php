<?php

declare(strict_types=1);

namespace Bake\Tests\Unit;

use Bake\Contract\LoggerInterface;
use Bake\Contract\RepositoryInterface;
use Bake\Service\DataProcessor;
use PHPUnit\Framework\TestCase;

class DataProcessorTest extends TestCase
{
    public function testProcessReturnsItemWhenFound(): void
    {
        $repo = $this->createMock(RepositoryInterface::class);
        $logger = $this->createMock(LoggerInterface::class);

        $item = ['id' => 1, 'name' => 'foo'];

        $repo->method('find')->willReturn($item);
        $logger->method('log');

        $processor = new DataProcessor($repo, $logger);
        $result = $processor->process(1);

        $this->assertEquals($item, $result);
    }

    public function testProcessReturnsNullWhenNotFound(): void
    {
        $repo = $this->createMock(RepositoryInterface::class);
        $logger = $this->createMock(LoggerInterface::class);

        $repo->method('find')->willReturn(null);
        $logger->method('log');

        $processor = new DataProcessor($repo, $logger);
        $result = $processor->process(99);

        $this->assertNull($result);
    }

    public function testProcessAllReturnsItems(): void
    {
        $repo = $this->createMock(RepositoryInterface::class);
        $logger = $this->createMock(LoggerInterface::class);

        $items = [['id' => 1], ['id' => 2]];

        $repo->method('findAll')->willReturn($items);
        $logger->method('log');

        $processor = new DataProcessor($repo, $logger);
        $result = $processor->processAll();

        $this->assertCount(2, $result);
    }

    public function testSaveDelegatesToRepository(): void
    {
        $repo = $this->createMock(RepositoryInterface::class);
        $logger = $this->createMock(LoggerInterface::class);

        $repo->method('save')->willReturn(true);

        $processor = new DataProcessor($repo, $logger);
        $result = $processor->save(['name' => 'bar']);

        $this->assertTrue($result);
    }

    public function testGetLastLogDelegatesToLogger(): void
    {
        $repo = $this->createMock(RepositoryInterface::class);
        $logger = $this->createMock(LoggerInterface::class);

        $message = 'last log message';
        $logger->method('getLastMessage')->willReturn($message);

        $processor = new DataProcessor($repo, $logger);
        $result = $processor->getLastLog();

        $this->assertEquals($message, $result);
    }

    public function testGetLoggerCodeReturnsCode(): void
    {
        $repo = $this->createMock(RepositoryInterface::class);
        $logger = $this->createMock(LoggerInterface::class);

        $code = 42;
        $logger->method('getCode')->willReturn($code);

        $processor = new DataProcessor($repo, $logger);
        $result = $processor->getLoggerCode();

        $this->assertEquals(42, $result);
    }
}
