// Smallest useful module: ANSI header, a parameter with a default, and one
// continuous assignment. Exercises parameter defaults and assign -> Comb.
module adder #(
    parameter int W = 8
) (
    input  logic [W-1:0] a,
    input  logic [W-1:0] b,
    output logic [W:0]   sum
);

    assign sum = a + b;

endmodule
