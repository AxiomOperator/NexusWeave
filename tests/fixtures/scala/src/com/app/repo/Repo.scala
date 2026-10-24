package com.app.repo

import com.app.models.User

class Repo {
  def find(id: Int): User = new User("u")
}
