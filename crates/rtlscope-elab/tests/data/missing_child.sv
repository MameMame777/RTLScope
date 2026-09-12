// Instantiates a module that is nowhere in the source set — an IP stub, or a
// file the user forgot to list. RTLScope must draw it as an opaque box and say
// so, not drop it and produce a diagram with a hole in it.
module missing_child (
    input  logic clk,
    output logic q
);

    black_box u_bb (.clk(clk), .q(q), .aux(undeclared_net));

endmodule
