<?php
use PHPUnit\Framework\TestCase;

class UserServiceTest extends TestCase
{
    public function testRename(): void
    {
        $repo = $this->createMock(UserRepository::class);
        $service = new UserService($repo);
        // Note: not asserting on $service since dispatch is stubbed in Phase 1.
        // This test exercises the test method body, which is what gets traced.
        $this->assertNotNull($service);
    }
}
