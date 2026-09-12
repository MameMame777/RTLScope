// Edge-triggered process with a synchronous active-low reset and an enable.
// Exercises Ff detection, ResetKind::Sync, and reads/writes derivation.
module counter #(
    parameter int W = 8
) (
    input  logic         clk,
    input  logic         rst_n,
    input  logic         en,
    output logic [W-1:0] count
);

    always_ff @(posedge clk) begin
        if (!rst_n)
            count <= {W{1'b0}};
        else if (en)
            count <= count + 1'b1;
    end

endmodule
