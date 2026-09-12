/// Counts while enabled, and says so once every LIMIT clocks.
module lights_Timer #(
    parameter int unsigned LIMIT = lights_Defs::TICKS
) (
    input var logic           i_clk  ,
    input var logic           i_rst_n,
    lights_Ticker.timer t  
);
    logic [8-1:0] count;

    always_ff @ (posedge i_clk, negedge i_rst_n) begin
        if (!i_rst_n) begin
            count <= 0;
        end else if (!t.enable) begin
            count <= 0;
        end else if (count == LIMIT - 1) begin
            count <= 0;
        end else begin
            count <= count + 1;
        end
    end

    always_comb t.tick = t.enable && (count == LIMIT - 1);
endmodule
//# sourceMappingURL=timer.sv.map
