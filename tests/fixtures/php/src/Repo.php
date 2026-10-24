<?php
namespace App;

use App\Models\User;

class Repo
{
    public function find(int $id): User { return new User(); }
}
