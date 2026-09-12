// What every part of the CPU agrees on: the widths, and the names of the
// operations. A package rather than a header, so the enum reaches every file
// that imports it — and reaches the waveform, where a row of `op` then reads
// LDI, SUB, JZ rather than 1, 3, 10.
//
// The machine is deliberately small: eight-bit data, four registers, a
// sixteen-word program. Small enough to read in one sitting, large enough to
// have every part a CPU has.
package cpu_pkg;
    localparam int DATA_W = 8;
    localparam int PC_W   = 4;

    // What an instruction does. The operands travel beside it as `rd`, `rs`
    // and `imm`, rather than packed into one word, so nothing has to be cast
    // back out of a bit slice to be understood.
    typedef enum logic [3:0] {
        NOP = 4'd0,
        LDI = 4'd1,   // rd <= imm
        ADD = 4'd2,   // rd <= rd + rs
        SUB = 4'd3,   // rd <= rd - rs
        AND = 4'd4,   // rd <= rd & rs
        OR  = 4'd5,   // rd <= rd | rs
        XOR = 4'd6,   // rd <= rd ^ rs
        MOV = 4'd7,   // rd <= rs
        OUT = 4'd8,   // out <= rs
        JMP = 4'd9,   // pc <= imm
        JZ  = 4'd10,  // pc <= imm, when the last result was zero
        HLT = 4'd11   // stop
    } opcode_t;
endpackage
