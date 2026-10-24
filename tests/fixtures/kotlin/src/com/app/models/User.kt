package com.app.models

interface Greeter {
    fun greet(): String
}

open class Person(val name: String) {
    fun display(): String = name
}

class User(name: String, val age: Int) : Person(name), Greeter {
    override fun greet(): String = "hi " + display()

    companion object {
        fun guest(): User = User("guest", 0)
    }
}
