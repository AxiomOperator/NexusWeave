package com.app.repo;

import com.app.models.User;

public class Repo implements Finder {
    public User find(String id) { return new User(); }
}
