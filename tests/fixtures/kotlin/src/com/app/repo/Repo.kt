package com.app.repo

import com.app.models.User

class Repo {
    fun find(id: Int): User {
        return User("u", id)
    }
}
