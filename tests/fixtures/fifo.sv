// Parameterised synchronous FIFO. Exercises the memory array (unpacked
// dimension), $clog2 constant folding, asynchronous reset, and the '0 fill
// literal that idiomatic SystemVerilog uses everywhere.
module fifo #(
    parameter W     = 8,
    parameter DEPTH = 16
) (
    input  logic         clk,
    input  logic         rst_n,
    input  logic         wr_en,
    input  logic [W-1:0] wr_data,
    input  logic         rd_en,
    output logic [W-1:0] rd_data,
    output logic         full,
    output logic         empty
);

    localparam int AW = $clog2(DEPTH);

    logic [W-1:0] mem [0:DEPTH-1];
    logic [AW:0]  wr_ptr;
    logic [AW:0]  rd_ptr;

    assign full  = (wr_ptr[AW-1:0] == rd_ptr[AW-1:0]) && (wr_ptr[AW] != rd_ptr[AW]);
    assign empty = (wr_ptr == rd_ptr);

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            wr_ptr <= '0;
        end else if (wr_en && !full) begin
            mem[wr_ptr[AW-1:0]] <= wr_data;
            wr_ptr <= wr_ptr + 1'b1;
        end
    end

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            rd_ptr  <= '0;
            rd_data <= '0;
        end else if (rd_en && !empty) begin
            rd_data <= mem[rd_ptr[AW-1:0]];
            rd_ptr  <= rd_ptr + 1'b1;
        end
    end

endmodule
