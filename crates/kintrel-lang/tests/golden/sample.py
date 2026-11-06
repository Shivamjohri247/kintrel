"""Module docstring: glue."""

import os, sys
from collections import OrderedDict as OD
from . import sibling

MAX_ITEMS = 100
TIMEOUT: float = 2.5

# Comment attached to the class.
@dataclass
class Cart:
    """Class docstring stays in the class's own atom."""

    currency = "USD"

    def __init__(self, items):
        self.items = items

    # Comment attached to price.
    @property
    def price(self):
        def helper():  # nested: part of price
            return 0
        return sum(i.price for i in self.items)

    class Meta:
        ordering = ["id"]


def total(carts):
    return sum(c.price for c in carts)


def total(carts, currency):
    return 0


print("top-level statement: glue")

if __name__ == "__main__":
    def main():  # inside an if: not top-level, so glue
        pass
    main()
