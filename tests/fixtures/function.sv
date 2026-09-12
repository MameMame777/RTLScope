// Functions, loops and the constant folding they lean on.
//
// Every construct here appears in real RTL and each one was, at some point,
// silently dropped: a function became an unsupported hole, a `for` never
// unrolled, `{VC, DT}` could not be folded, and `input logic [7:0] b0, b1`
// lost `b1` altogether.
module function_test #(
    parameter logic [5:0] DT = 6'h22,
    parameter logic [1:0] VC = 2'h1,
    parameter int         LANES = 4
) (
    input  wire         clk,
    input  wire  [7:0]  byte_in,
    input  wire         c0, c1,
    input  wire  [31:0] packed_lanes,
    output logic [7:0]  header,
    output logic [31:0] ones,
    output logic [9:0]  code,
    output logic [7:0]  lane_sum,
    output logic [7:0]  rotated
);
    // A concatenation of sized parameters: the widths decide the answer.
    localparam logic [7:0] DI = {VC, DT};

    // Counts bits with a `for` whose bound is a literal, assigning to the
    // function's own name and using a compound assignment to do it.
    function automatic int count_ones8(input logic [7:0] value);
        count_ones8 = 0;
        for (int idx = 0; idx < 8; idx++) begin
            count_ones8 += value[idx];
        end
    endfunction

    // `case` assigning to the function name, and two arguments sharing one
    // type declaration.
    function automatic logic [9:0] control_code(input logic a, b);
        case ({b, a})
            2'b00:   control_code = 10'b1101010100;
            2'b01:   control_code = 10'b0010101011;
            default: control_code = 10'b1010101011;
        endcase
    endfunction

    // A local variable, and a result delivered by `return`.
    function automatic logic [7:0] sum_lanes(input logic [31:0] lanes);
        logic [7:0] total;
        total = 8'd0;
        for (int i = 0; i < LANES; i++) begin
            total += lanes[i*8 +: 8];
        end
        return total;
    endfunction

    // A loop bounded by an argument: unrollable only where the caller passes a
    // constant, which is what specialising the body at each call site gives.
    function automatic logic [7:0] rotate_right8(input logic [7:0] value,
                                                 input logic [2:0] amount);
        logic [7:0] result;
        result = value;
        for (int idx = 0; idx < amount; idx++) begin
            result = {result[0], result[7:1]};
        end
        return result;
    endfunction

    always_comb begin
        // A variable the block declares for itself, not a module net.
        // (`automatic` would say the same thing; Icarus cannot parse it.)
        logic [7:0] scratch;
        scratch = byte_in ^ DI;
        header  = scratch;
    end

    assign ones     = count_ones8(byte_in);
    assign code     = control_code(c0, c1);
    assign lane_sum = sum_lanes(packed_lanes);
    assign rotated  = rotate_right8(byte_in, 3'd3);
endmodule
