// Three-level hierarchy exercising all three connection styles in one design.
// This is the primary block-diagram fixture: hier_top is what Phase 1 draws,
// drills into, and rounds through Sugiyama layout.
module hier_alu #(
    parameter int W = 32
) (
    input  logic [W-1:0] a,
    input  logic [W-1:0] b,
    input  logic [1:0]   op,
    output logic [W-1:0] y
);

    always_comb begin
        case (op)
            2'd0:    y = a + b;
            2'd1:    y = a - b;
            2'd2:    y = a & b;
            default: y = a | b;
        endcase
    end

endmodule

module hier_regfile #(
    parameter int W  = 32,
    parameter int AW = 4
) (
    input  logic          clk,
    input  logic          we,
    input  logic [AW-1:0] waddr,
    input  logic [W-1:0]  wdata,
    input  logic [AW-1:0] raddr,
    output logic [W-1:0]  rdata
);

    logic [W-1:0] regs [0:(1 << AW) - 1];

    always_ff @(posedge clk) begin
        if (we)
            regs[waddr] <= wdata;
    end

    assign rdata = regs[raddr];

endmodule

module hier_ctrl (
    input  logic clk,
    input  logic rst_n,
    input  logic start,
    output logic busy
);

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            busy <= 1'b0;
        else
            busy <= start;
    end

endmodule

module hier_top #(
    parameter int W = 32
) (
    input  logic         clk,
    input  logic         rst_n,
    input  logic         start,
    input  logic [1:0]   op,
    input  logic [3:0]   raddr,
    input  logic [3:0]   waddr,
    input  logic         we,
    output logic [W-1:0] result,
    output logic         busy
);

    logic [W-1:0] rf_rdata;
    logic [W-1:0] alu_y;

    // Named connections.
    hier_alu #(.W(W)) u_alu (
        .a  (rf_rdata),
        .b  (rf_rdata),
        .op (op),
        .y  (alu_y)
    );

    // Positional connections.
    hier_regfile #(.W(W), .AW(4)) u_rf (
        clk, we, waddr, alu_y, raddr, rf_rdata
    );

    // Wildcard: every port binds to the same-named net in this scope.
    hier_ctrl u_ctrl (.*);

    assign result = alu_y;

endmodule
