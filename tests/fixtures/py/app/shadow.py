import json


class Codec:
    def dumps(self, x):
        return str(x)


def uses_json(x):
    return json.dumps(x)


def on_created(x):
    return x


HANDLERS = {"created": on_created}


def describe(name):
    return f"{name.upper()} {render_name(name)}"


def render_name(name):
    return name
