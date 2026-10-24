package com.app.repo;

import com.app.models.User;

public interface Finder {
    User find(String id);
}
