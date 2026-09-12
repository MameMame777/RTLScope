// Parameter propagation across two levels. params_sub is instantiated twice
// with different bindings, so elaboration must specialise it into two distinct
// modules rather than one module with a parameter still in it.
module params_subsub #(
    parameter int W = 4
) (
    input  logic [W-1:0] i,
    output logic [W-1:0] o
);

    assign o = ~i;

endmodule

module params_sub #(
    parameter int W = 8
) (
    input  logic [W-1:0] i,
    output logic [W-1:0] o
);

    params_subsub #(.W(W)) u_ss (.i(i), .o(o));

endmodule

module params_top (
    input  logic [15:0] a,
    input  logic [7:0]  b,
    output logic [15:0] ya,
    output logic [7:0]  yb
);

    params_sub #(.W(16)) u_wide    (.i(a), .o(ya));
    params_sub           u_default (.i(b), .o(yb));

endmodule
