<?php
interface UserRepository
{
    public function find(int $id): ?User;
    public function save(User $user): void;
}
