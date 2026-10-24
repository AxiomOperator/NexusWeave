from app.repo import Repo


def make_repo():
    return Repo()


def make_and_find():
    r = make_repo()
    return r.find(1)
