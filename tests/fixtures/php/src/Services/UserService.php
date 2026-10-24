<?php
namespace App\Services;

use App\Models\User;
use App\Repo as R;

class UserService
{
    public function __construct(private R $repo) {}

    public function run(): void
    {
        $u = $this->repo->find(1);
        $u->greet();
        $u->save();
        $x = new User();
        strlen("x");
    }
}
