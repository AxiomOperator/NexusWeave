package com.app

import com.app.models.User
import com.app.repo.Repo

fun helper(x: Int): Int = x + 1

class Service(private val repo: Repo) {
    fun handle(id: Int): String {
        val user = repo.find(helper(id))
        val g = User.guest()
        return user.greet() + g.display()
    }

    fun direct(u: User): String {
        return u.greet()
    }
}
