<?php

declare(strict_types=1);

namespace Bake\Contract;

interface LoggerInterface
{
    public function log(string $level, string $message): void;

    public function getLastMessage(): ?string;

    public function getCode(): int;
}
