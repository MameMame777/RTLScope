// Three-stage valid/data pipeline. This is the fixture Phase 4 stage detection
// is written against: the valid chain is named to the *_d<N> convention the
// heuristic looks for, and the register-adjacency graph through it is a
// straight line with no feedback.
module pipeline3 #(
    parameter int W = 16
) (
    input  logic         clk,
    input  logic         rst_n,
    input  logic         in_valid,
    input  logic [W-1:0] in_data,
    output logic         out_valid,
    output logic [W-1:0] out_data
);

    logic         valid_d1, valid_d2, valid_d3;
    logic [W-1:0] data_d1,  data_d2,  data_d3;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            valid_d1 <= 1'b0;
            valid_d2 <= 1'b0;
            valid_d3 <= 1'b0;
            data_d1  <= '0;
            data_d2  <= '0;
            data_d3  <= '0;
        end else begin
            valid_d1 <= in_valid;
            data_d1  <= in_data;
            valid_d2 <= valid_d1;
            data_d2  <= data_d1 + 1'b1;
            valid_d3 <= valid_d2;
            data_d3  <= data_d2 ^ {W{1'b1}};
        end
    end

    assign out_valid = valid_d3;
    assign out_data  = data_d3;

endmodule
