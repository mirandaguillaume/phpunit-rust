<?php
use PHPUnit\Framework\TestCase;

class MoneyTest extends TestCase
{
    public function testAdd(): void
    {
        $a = new Money(100, 'EUR');
        $b = new Money(50, 'EUR');
        $sum = $a->add($b);
        $this->assertSame(150, $sum->amount());
    }

    public function testAccessors(): void
    {
        $m = new Money(42, 'USD');
        $this->assertSame(42, $m->amount());
        $this->assertSame('USD', $m->currency());
    }
}
