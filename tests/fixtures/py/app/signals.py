from pyee import EventEmitter

ee = EventEmitter()


@ee.on("order:paid")
def ship(order):
    return order


def pay(order):
    ee.emit("order:paid", order)
