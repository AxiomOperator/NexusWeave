<?php
use App\Services\UserService;

Route::get('/users', [UserService::class, 'run']);
