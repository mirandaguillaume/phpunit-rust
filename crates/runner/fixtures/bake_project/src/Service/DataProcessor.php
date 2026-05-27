<?php

declare(strict_types=1);

namespace Bake\Service;

use Bake\Contract\LoggerInterface;
use Bake\Contract\RepositoryInterface;

class DataProcessor
{
    public function __construct(
        private RepositoryInterface $repository,
        private LoggerInterface $logger,
    ) {
    }

    public function process(int $id): ?array
    {
        $item = $this->repository->find($id);
        if ($item === null) {
            $this->logger->log('warning', "Item $id not found");
            return null;
        }
        $this->logger->log('info', "Item $id processed");
        return $item;
    }

    public function processAll(): array
    {
        $items = $this->repository->findAll();
        $this->logger->log('info', sprintf('%d items processed', count($items)));
        return $items;
    }

    public function save(array $data): bool
    {
        return $this->repository->save($data);
    }

    public function getLastLog(): ?string
    {
        return $this->logger->getLastMessage();
    }

    public function getLoggerCode(): int
    {
        return $this->logger->getCode();
    }
}
