// Package cart prices carts.
package cart

import (
	"fmt"
	"strings"
)

// MaxItems caps a cart.
const MaxItems = 100

var registry = map[string]int{}

// Cart holds lines.
type Cart struct {
	Lines []Line
}

type Line struct{ Qty int }

// Total sums the lines.
func (c *Cart) Total() int {
	n := 0
	for _, l := range c.Lines {
		n += l.Qty
	}
	return n
}

func (l Line) String() string { return fmt.Sprint(l.Qty) }

func (c *Cart) String() string { return strings.Repeat("x", c.Total()) }

func main() {}
