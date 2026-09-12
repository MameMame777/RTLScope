

module lights_Show
    import lights_Defs::*;
(
    input var logic                      i_clk  ,
    input var logic                      i_rst_n,
    lights_Ticker.watch            t      ,
    output var logic           [LEDS-1:0] o_led  
);
    always_ff @ (posedge i_clk, negedge i_rst_n) begin
        if (!i_rst_n) begin
            o_led <= first_led();
        end else if (t.enable && t.tick) begin
            o_led <= {o_led[LEDS - 2:0], o_led[LEDS - 1]};
        end
    end
endmodule
//# sourceMappingURL=show.sv.map
