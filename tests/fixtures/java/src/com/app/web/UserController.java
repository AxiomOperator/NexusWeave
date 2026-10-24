package com.app.web;

import com.app.repo.Repo;

@RestController
@RequestMapping("/api")
public class UserController {
    private final Repo repo = new Repo();

    @GetMapping("/users/{id}")
    public Object get(String id) { return repo.find(id); }
}
