<?php
final class UserService
{
    public function __construct(private UserRepository $repo) {}

    public function rename(int $id, string $newName): User
    {
        $user = $this->repo->find($id);
        if (!$user) { throw new \RuntimeException("not found: $id"); }
        $renamed = new User($user->id, $newName);
        $this->repo->save($renamed);
        return $renamed;
    }
}
