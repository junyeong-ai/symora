"""Geometry primitives consumed across the workspace boundary."""

PI = 3.14159


def area(radius: float) -> float:
    return PI * radius * radius


class Circle:
    def __init__(self, radius: float) -> None:
        self.radius = radius

    def perimeter(self) -> float:
        return 2 * PI * self.radius
