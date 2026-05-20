<?php

use PHPUnit\Framework\TestCase;

class AnimalTest extends TestCase
{
    public function testNarrowsToDog(): void
    {
        $a = $this->makeAnimal('dog');
        if ($a instanceof Dog) {
            $a->bark();
        }
    }

    private function makeAnimal(string $kind): Animal
    {
        if ($kind === 'dog') {
            return new Dog();
        }
        return new Cat();
    }
}
