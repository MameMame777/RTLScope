// Connections that are expressions rather than plain net names.
//
// `.rst_n(!rst)` and `.pair({hi, lo})` are logic written where a wire would go.
// Elaboration gives each one the net someone could have declared by hand, so
// that the instance keeps its edge in the block diagram instead of showing an
// open pin.
module connect_expr_child (
    input  wire       rst_n,
    input  wire [1:0] pair,
    input  wire       sel,
    output logic      q
);
    always_comb q = sel ? pair[1] : (pair[0] & rst_n);
endmodule

module connect_expr_top (
    input  wire  rst,
    input  wire  hi,
    input  wire  lo,
    input  wire  a,
    input  wire  b,
    output logic q
);
    connect_expr_child u_child (
        .rst_n(!rst),
        .pair({hi, lo}),
        .sel(a && b),
        .q(q)
    );
endmodule
