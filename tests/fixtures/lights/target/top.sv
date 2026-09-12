/// A start button, a timer, a little state machine, and a row of lights.
///
/// Small on purpose: three modules under one clock, so every view has
/// something to show and nothing to hide it behind. What they share — how
/// many lights, how fast — is in the `Defs` package, and the timer's two
/// wires travel as one `Ticker` bundle.
module lights_Top (
    input  var logic                         i_clk  ,
    input  var logic                         i_rst_n,
    input  var logic                         i_start,
    output var logic                         o_busy ,
    output var logic [lights_Defs::LEDS-1:0] o_led  
);
    lights_Ticker t ();

    lights_Timer #(
        .LIMIT (lights_Defs::TICKS)
    ) u_timer (
        .i_clk   (i_clk  ),
        .i_rst_n (i_rst_n),
        .t       (t      )
    );

    lights_Control u_ctrl (
        .i_clk   (i_clk  ),
        .i_rst_n (i_rst_n),
        .i_start (i_start),
        .t       (t      ),
        .o_busy  (o_busy )
    );

    lights_Show u_show (
        .i_clk   (i_clk  ),
        .i_rst_n (i_rst_n),
        .t       (t      ),
        .o_led   (o_led  )
    );
endmodule
//# sourceMappingURL=top.sv.map
