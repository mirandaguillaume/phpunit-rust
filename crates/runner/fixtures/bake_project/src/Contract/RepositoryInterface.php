<?php

declare(strict_types=1);

namespace Bake\Contract;

interface RepositoryInterface extends ReadableInterface, WritableInterface
{
    public function count(): int;

    public static function tableName(): string;
}
