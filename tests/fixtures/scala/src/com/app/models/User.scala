package com.app.models

trait Greeter {
  def greet(): String
}

class Person(val name: String) {
  def display(): String = name
}

class User(name: String) extends Person(name) with Greeter {
  def greet(): String = "hi " + display()
}

object User {
  def guest(): User = new User("guest")
}
