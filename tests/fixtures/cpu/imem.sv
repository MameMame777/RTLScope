// The program, as a read-only memory: a `case` on the address, which is what
// a small instruction ROM synthesises to.
//
// The program counts down from five, showing each value on `out` on the way,
// and halts when the count reaches zero:
//
//     0  LDI r0, 5
//     1  LDI r1, 1
//     2  OUT r0          <- the loop
//     3  SUB r0, r1
//     4  JZ  6           <- leave when r0 - r1 was zero
//     5  JMP 2
//     6  HLT
//
// Each field comes out on its own port. The alternative — one sixteen-bit
// word sliced apart by the datapath — needs the opcode cast back into its
// enum to be named anywhere, and a cast is one more thing to get wrong.
module imem
    import cpu_pkg::*;
(
    input  logic [PC_W-1:0]   addr,
    output opcode_t           op,
    output logic [1:0]        rd,
    output logic [1:0]        rs,
    output logic [DATA_W-1:0] imm
);
    always_comb begin
        // Every field has a value on every address, so nothing is a latch.
        op  = NOP;
        rd  = 2'd0;
        rs  = 2'd0;
        imm = {DATA_W{1'b0}};
        case (addr)
            4'd0: begin op = LDI; rd = 2'd0; imm = 8'd5; end
            4'd1: begin op = LDI; rd = 2'd1; imm = 8'd1; end
            4'd2: begin op = OUT; rs = 2'd0; end
            4'd3: begin op = SUB; rd = 2'd0; rs = 2'd1; end
            4'd4: begin op = JZ;  imm = 8'd6; end
            4'd5: begin op = JMP; imm = 8'd2; end
            4'd6: begin op = HLT; end
            default: op = NOP;
        endcase
    end
endmodule
