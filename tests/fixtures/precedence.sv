// Operator precedence, which is not in the syntax tree.
//
// `sv-parser` builds a pure parse tree and applies no precedence at all: a run
// of binary operators arrives leaning entirely to the right, so `a == b && c ==
// d` reaches the lowering as `a == (b && (c == d))`. Nothing downstream can
// notice — the reads are the same signals either way — so the wrong grouping
// shows up much later, as a constant folded wrong or a condition that reads
// backwards. Every line here has a shape that changes meaning if the run is
// rebuilt naively.
module precedence (
    input  wire [3:0]  a, b, c, d,
    output logic       eq_and,
    output logic [7:0] add_mul,
    output logic [3:0] or_and,
    output logic       cmp_chain,
    output logic [7:0] shift_add,
    output logic       parenthesised
);
    assign eq_and        = a == b && c == d;   // (a==b) && (c==d)
    assign add_mul       = a + b * c;          // a + (b*c)
    assign or_and        = a | b & c;          // a | (b&c)
    assign cmp_chain     = a < b == c;         // (a<b) == c, left to right
    assign shift_add     = a + b << c;         // (a+b) << c
    assign parenthesised = (a == b) == (c == d);
endmodule
