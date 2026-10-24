class User:
    def __init__(self, name):
        self.name = name

    def greet(self):
        return "hi"

    @property
    def display(self) -> str:
        return self.name


class Admin(User):
    def ban(self):
        self.greet()
