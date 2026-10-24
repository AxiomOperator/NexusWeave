import os
from app.repo import Repo
from app import models


class Service:
    def __init__(self, repo: Repo):
        self.repo = repo

    def get(self, id):
        u = self.repo.find(id)
        u.greet()
        print(u.display)
        a = models.Admin("x")
        a.greet()
        os.path.join("a", "b")
        return helper(u)


def helper(u):
    return u


def run_factory():
    from app.factory import make_and_find
    u = make_and_find()
    u.greet()
