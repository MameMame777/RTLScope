// A cycle of wires, and a chain of them that is not one.
//
// `knot_a` and `knot_b` hold each other up with nothing clocked in between, so
// neither has a settled value: synthesis refuses it and simulation answers with
// whichever order it happened to evaluate in. It is a real bug and the kind a
// reader cannot see by looking at either line on its own — the two assignments
// are innocent apart and wrong together.
//
// `step_one` through `step_three` are the half that must not be reported. They
// are combinational too, and long enough to look like the same shape to a
// careless walk, but the value goes one way and stops.

module comb_loop (
    input  logic       clk,
    input  logic       rst_n,
    input  logic       seed,
    input  logic       en,
    output logic       out_knot,
    output logic       out_chain
);

    logic knot_a;
    logic knot_b;

    logic step_one;
    logic step_two;
    logic step_three;

    // The loop: each is the other's input.
    assign knot_a = knot_b & en;
    assign knot_b = knot_a | seed;

    // The chain: three wires deep, and every one of them settles.
    assign step_one   = seed & en;
    assign step_two   = step_one ^ en;
    assign step_three = step_two | seed;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            out_knot  <= 1'b0;
            out_chain <= 1'b0;
        end else begin
            out_knot  <= knot_a;
            out_chain <= step_three;
        end
    end

endmodule
