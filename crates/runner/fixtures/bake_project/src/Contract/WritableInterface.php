<?php

declare(strict_types=1);

namespace Bake\Contract;

interface WritableInterface
{
    public function save(array $data): bool;

    public function delete(int $id): void;
}
