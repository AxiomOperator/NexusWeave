class User:
    def __init__(self, name):
        self.name = name

    def greet(self):
        return "hi"


class Admin(User):
    def ban(self):
        self.greet()
