<?php

declare(strict_types=1);

namespace Bake\Contract;

interface ReadableInterface
{
    public function find(int $id): ?array;

    public function findAll(): array;
}
