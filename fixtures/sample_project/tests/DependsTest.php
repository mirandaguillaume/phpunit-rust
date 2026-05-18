<?php

declare(strict_types=1);

namespace Sample\Tests;

use PHPUnit\Framework\Attributes\Depends;
use PHPUnit\Framework\TestCase;
use Sample\Repository;

final class DependsTest extends TestCase
{
    public function testCreatesEmptyRepository(): Repository
    {
        $repo = new Repository();
        $this->assertSame(0, $repo->count());
        return $repo;
    }

    #[Depends('testCreatesEmptyRepository')]
    public function testCanAddItem(Repository $repo): Repository
    {
        $id = $repo->add('first');
        $this->assertSame(0, $id);
        $this->assertSame(1, $repo->count());
        return $repo;
    }

    #[Depends('testCanAddItem')]
    public function testRetainsItemAfterAdd(Repository $repo): void
    {
        $this->assertSame('first', $repo->get(0));
    }
}
