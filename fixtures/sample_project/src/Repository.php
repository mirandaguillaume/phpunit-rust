<?php

declare(strict_types=1);

namespace Sample;

final class Repository
{
    /** @var array<int, string> */
    private array $items = [];

    public function add(string $item): int
    {
        $this->items[] = $item;
        return array_key_last($this->items);
    }

    public function get(int $id): string
    {
        return $this->items[$id] ?? throw new \OutOfBoundsException("no item with id {$id}");
    }

    public function count(): int
    {
        return count($this->items);
    }
}
