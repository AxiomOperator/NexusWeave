package com.app

import com.app.models.{User, Person}
import com.app.repo.Repo

object Util {
  def helper(x: Int): Int = x + 1
}

class Service(repo: Repo) {
  def handle(id: Int): String = {
    val user = repo.find(Util.helper(id))
    user.greet()
  }

  def direct(u: User): String = u.display()
}
