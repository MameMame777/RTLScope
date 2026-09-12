// generate for and generate if. Exercises static unrolling, genvar-derived
// names, and the rule that a construct duplicated N times keeps the Span of
// the single line the author wrote.
module genblk #(
    parameter int TAPS   = 4,
    parameter int W      = 8,
    parameter bit BYPASS = 1'b0
) (
    input  logic         clk,
    input  logic [W-1:0] din,
    output logic [W-1:0] dout
);

    logic [W-1:0] tap [0:TAPS-1];
    genvar i;

    generate
        if (BYPASS) begin : g_bypass
            assign dout = din;
        end else begin : g_chain
            for (i = 0; i < TAPS; i = i + 1) begin : g_tap
                if (i == 0) begin : g_first
                    always_ff @(posedge clk) tap[0] <= din;
                end else begin : g_rest
                    always_ff @(posedge clk) tap[i] <= tap[i-1];
                end
            end
            assign dout = tap[TAPS-1];
        end
    endgenerate

endmodule
