// The arithmetic, with no state of its own: one `case` on the operation.
//
// `a` is the destination register's current value and `b` the source's, so
// every two-operand instruction is `a op b` and LDI is the immediate. MOV is
// `b`; the default keeps `a`, which is what an instruction that writes no
// register leaves the datapath holding.
module alu
    import cpu_pkg::*;
(
    input  opcode_t           op,
    input  logic [DATA_W-1:0] a,
    input  logic [DATA_W-1:0] b,
    input  logic [DATA_W-1:0] imm,
    output logic [DATA_W-1:0] result,
    output logic              zero
);
    always_comb begin
        case (op)
            LDI:     result = imm;
            ADD:     result = a + b;
            SUB:     result = a - b;
            AND:     result = a & b;
            OR:      result = a | b;
            XOR:     result = a ^ b;
            MOV:     result = b;
            default: result = a;
        endcase
    end

    assign zero = (result == {DATA_W{1'b0}});
endmodule
