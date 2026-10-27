"""Geometry primitives consumed across the workspace boundary."""

PI = 3.14159


def area(radius: float) -> float:
    def squared(value: float) -> float:
        return value * value

    return PI * squared(radius)


class Circle:
    def __init__(self, radius: float) -> None:
        self.radius = radius

    def perimeter(self) -> float:
        return 2 * PI * self.radius
