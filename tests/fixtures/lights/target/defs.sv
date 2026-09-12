/// What every module of the sequence agrees on.
package lights_Defs;
    /// How many lights walk.
    localparam int unsigned LEDS = 4;

    /// Clocks between one step of the walk and the next.
    localparam int unsigned TICKS = 24;

    /// The light the walk starts from.
    function automatic logic [LEDS-1:0] first_led() ;
        return 1;
    endfunction
endpackage
//# sourceMappingURL=defs.sv.map
