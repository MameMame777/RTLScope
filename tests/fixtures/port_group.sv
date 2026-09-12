// `input logic [7:0] a, b` — one header, two ports.
//
// IEEE 1800 §23.2.2.3 says the second port inherits the first's type. Reading
// only the direction left `b` one bit wide with no diagnostic, which is exactly
// the kind of quiet wrongness the subset rules forbid.
module port_group (
    input  wire  clk,
    input  wire  [7:0] a, b,
    output logic [7:0] wide, also_wide
);
    always_ff @(posedge clk) begin
        wide      <= a;
        also_wide <= b;
    end
endmodule
