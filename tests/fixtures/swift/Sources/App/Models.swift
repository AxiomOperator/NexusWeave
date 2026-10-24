protocol Greeter {
    func greet() -> String
}

class Person {
    let name: String
    init(name: String) { self.name = name }
    func display() -> String { return name }
}

class User: Person, Greeter {
    func greet() -> String { return "hi " + self.display() }
}

extension User {
    func shout() -> String { return greet() }
}
