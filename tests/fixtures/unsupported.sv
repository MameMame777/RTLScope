// Every construct here is legal SystemVerilog and deliberately outside the
// RTLScope subset. The diagnostics this file produces are a golden test: D2
// requires the tool to say what it skipped, not to quietly mis-model it. A
// silent pass over this file is a bug, and so is a crash.
module unsupported (
    input  logic       clk,
    input  logic [3:0] sel,
    output logic [3:0] q
);

    logic [3:0] shadow;

    // `forever` says nothing about how long it runs, so there is no number of
    // copies of the body that would be the hardware.
    always @(posedge clk) begin
        forever begin
            shadow = shadow + 4'h1;
        end
    end

    // A delay describes simulation time, which the hardware does not have.
    always @(posedge clk) begin
        #10 shadow = 4'h0;
    end

    // casex is a synthesis hazard; RTLScope refuses to guess what it means.
    always @(posedge clk) begin
        casex (sel)
            4'b1xxx: q <= 4'h8;
            4'b01xx: q <= 4'h4;
            default: q <= 4'h0;
        endcase
    end

endmodule

module unsupported_child #(
    parameter int W = 8
) (
    input  logic [W-1:0] i,
    output logic [W-1:0] o
);

    assign o = i;

endmodule

// defparam is rejected outright (D2): it makes a parameter binding depend on
// where you look rather than on the instantiation.
module unsupported_defparam (
    input  logic [7:0] i,
    output logic [7:0] o
);

    unsupported_child u_c (.i(i), .o(o));
    defparam u_c.W = 8;

endmodule
