from .models import User


class Repo:
    def find(self, id) -> User:
        return User(id)
