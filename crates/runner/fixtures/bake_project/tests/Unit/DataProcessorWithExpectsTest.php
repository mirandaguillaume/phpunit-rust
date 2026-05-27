<?php

declare(strict_types=1);

namespace Bake\Tests\Unit;

use Bake\Contract\LoggerInterface;
use Bake\Contract\RepositoryInterface;
use Bake\Service\DataProcessor;
use PHPUnit\Framework\TestCase;

/**
 * Tests using expects($this->once()), with(), and interleaved patterns.
 */
class DataProcessorWithExpectsTest extends TestCase
{
    // Pattern: mock created in setUp as class property (NOT baked — verifies fallback)
    private RepositoryInterface $repo;
    private LoggerInterface $logger;
    private DataProcessor $processor;

    protected function setUp(): void
    {
        $this->repo = $this->createMock(RepositoryInterface::class);
        $this->logger = $this->createMock(LoggerInterface::class);
        $this->processor = new DataProcessor($this->repo, $this->logger);
    }

    public function testFindCalledOnceWithId(): void
    {
        $item = ['id' => 5, 'name' => 'bar'];
        $this->repo->expects($this->once())->method('find')->willReturn($item);
        $this->logger->method('log');

        $result = $this->processor->process(5);
        $this->assertEquals($item, $result);
    }

    public function testSaveExpectsOnce(): void
    {
        $this->repo->expects($this->once())->method('save')->willReturn(true);
        $result = $this->processor->save(['x' => 1]);
        $this->assertTrue($result);
    }

    public function testFindAllCalledOnce(): void
    {
        $items = [['id' => 10], ['id' => 20], ['id' => 30]];
        $this->repo->expects($this->once())->method('findAll')->willReturn($items);
        $this->logger->method('log');

        $result = $this->processor->processAll();
        $this->assertCount(3, $result);
    }
}
