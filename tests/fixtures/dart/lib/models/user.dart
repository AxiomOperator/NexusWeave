abstract class Greeter {
  String greet();
}

class Person {
  final String name;
  Person(this.name);
  String display() => name;
}

class User extends Person implements Greeter {
  User(String name) : super(name);

  @override
  String greet() {
    return 'hi ' + display();
  }
}
