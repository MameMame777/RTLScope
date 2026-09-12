// Four registers, read two at a time and written one at a time.
//
// Not reset: a register file is a memory, and a memory comes up holding
// whatever it holds. The program is expected to load what it reads before it
// reads it, which this one does — `LDI` twice before anything else.
module regfile
    import cpu_pkg::*;
(
    input  logic              clk,
    input  logic              we,
    input  logic [1:0]        waddr,
    input  logic [DATA_W-1:0] wdata,
    input  logic [1:0]        raddr_a,
    input  logic [1:0]        raddr_b,
    output logic [DATA_W-1:0] rdata_a,
    output logic [DATA_W-1:0] rdata_b
);
    logic [DATA_W-1:0] regs [0:3];

    always_ff @(posedge clk) begin
        if (we)
            regs[waddr] <= wdata;
    end

    assign rdata_a = regs[raddr_a];
    assign rdata_b = regs[raddr_b];
endmodule
