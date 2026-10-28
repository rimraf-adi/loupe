package demo;

/** A shape. */
public interface Shape {
    double area();
}

public enum Color {
    RED, GREEN
}

/** A circle. */
public class Circle implements Shape {
    private final double radius;

    public Circle(double radius) {
        this.radius = radius;
    }

    @Override
    public double area() {
        return Math.PI * radius * radius;
    }

    private static void helper() {}

    void packagePrivate() {}

    protected String describe() {
        return "circle";
    }
}
