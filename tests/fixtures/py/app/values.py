"""Names used as values rather than called: exceptions, returned closures, class qualifiers,
bound methods and properties passed along."""

from .models import User


class NotFound(Exception):
    pass


class Registry:
    item_class = User

    @classmethod
    def build(cls):
        return cls()

    @property
    def size(self):
        return 0

    @size.setter
    def size(self, value):
        pass

    def resize(self):
        self.size = 3
        self.count = 0

    def on_add(self):
        pass

    def handlers(self):
        return (self.on_add, Registry.build)

    def describe(self):
        return format(self.size)


def lookup(key):
    try:
        return Registry.build()
    except (NotFound, KeyError):
        raise NotFound


def logged(view):
    def wrapped_view():
        return view()

    return wrapped_view


def unknown_receiver(thing):
    # an untyped receiver: a member read never guesses which class `thing` is
    return thing.on_add
