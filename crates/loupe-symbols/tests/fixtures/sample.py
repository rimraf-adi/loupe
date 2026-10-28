"""Module docstring is not a symbol."""
import os


def top_level(a, b=2):
    """Add two numbers.

    Returns the sum.
    """
    def inner_helper():
        pass
    return a + b


def _private_function():
    pass


# A comment block
# above the class.
class Animal:
    """An animal."""

    species = "unknown"

    def __init__(self, name):
        self.name = name

    def speak(self):
        raise NotImplementedError

    @staticmethod
    def create(kind):
        return Animal()

    class Inner:
        def nested_method(self):
            pass


@decorator
class Dog(Animal):
    def speak(self):
        return "woof"

    def __repr__(self):
        return "Dog"
