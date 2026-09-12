// Line 1.
`include "defs.svh"
// Line 3.
module with_include (
    input  logic [`DATA_W-1:0] din,
    output logic [`DATA_W-1:0] dout
);

    assign dout = din;

endmodule
