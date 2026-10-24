from flask import Flask
from app.models import User

app = Flask(__name__)


@app.route("/users/<id>", methods=["POST"])
def create_user(id):
    return User(id)


@app.get("/health")
def health():
    return "ok"


class Guest(User):
    def greet(self):
        return "hey"
