<?php
final class Money
{
    public function __construct(
        private readonly int $amount,
        private readonly string $currency,
    ) {}

    public function amount(): int { return $this->amount; }
    public function currency(): string { return $this->currency; }

    public function add(Money $other): Money
    {
        if ($this->currency !== $other->currency) {
            throw new InvalidArgumentException('currency mismatch');
        }
        return new Money($this->amount + $other->amount, $this->currency);
    }
}
