<?php

declare(strict_types=1);

namespace App\Tests;

use App\Service;
use PHPUnit\Framework\TestCase;

class ServiceTest extends TestCase
{
    public function testGo(): void
    {
        $service = new Service();
        $service->go();
    }
}
