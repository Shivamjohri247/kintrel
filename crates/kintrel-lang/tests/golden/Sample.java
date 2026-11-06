package shop;

import java.util.List;
import static java.lang.Math.max;

/** A cart. */
public class Cart extends Base implements Priced {
    private final List<Line> lines;

    public Cart(List<Line> lines) {
        this.lines = lines;
    }

    /** Total in cents. */
    @Override
    public int total() {
        return lines.size();
    }

    public int total(int discount) {
        return max(0, total() - discount);
    }

    static class Line {
        int qty;
    }
}

interface Priced {
    int ZERO = 0;
    int total();
}

enum Currency { USD, EUR }

record Money(int cents) {}
