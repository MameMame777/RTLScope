// A construct that arrives through a macro has no text of its own to rewrite:
// what the author wrote is the macro call. `from_macro` below is declared by
// `A_WIRE, so an editor asked to delete or retype it would have to edit the
// `define — in another file, shared with every other use of it.
//
// The net beside it is written out, and does have an extent. The two together
// are the whole point of the check: the difference has to be visible.
`define A_WIRE logic from_macro

module from_macro (
    input  logic a,
    output logic b
);

    `A_WIRE;
    logic written_out;

    assign from_macro = a;
    assign written_out = from_macro;
    assign b = written_out;

endmodule
