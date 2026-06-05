"""Cross-package consumer: every call below crosses the workspace boundary."""

from fixture_core.geometry import Circle, area


def describe(radius: float) -> str:
    circle = Circle(radius)
    return f"area={area(radius)} perimeter={circle.perimeter()}"
