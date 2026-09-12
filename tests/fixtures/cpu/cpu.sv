// The processor: a sequencer, a datapath, and the program it runs.
//
// Three instances and the wires between them, which is the whole of what this
// module says — every decision is made one level down. Reading it from the
// top: `imem` holds the instruction out at the address `datapath` gives it,
// `datapath` latches that instruction and does what `control` says, and
// `control` looks at the latched instruction and the zero flag to decide what
// to say next.
module cpu
    import cpu_pkg::*;
(
    input  logic              clk,
    input  logic              rst_n,
    output logic [DATA_W-1:0] out,
    output logic              halted
);
    logic [PC_W-1:0]   pc;
    opcode_t           insn_op;
    logic [1:0]        insn_rd;
    logic [1:0]        insn_rs;
    logic [DATA_W-1:0] insn_imm;

    opcode_t           op;
    logic              zero;

    logic ir_load;
    logic pc_inc;
    logic pc_load;
    logic reg_we;
    logic out_we;

    imem u_imem (
        .addr (pc),
        .op   (insn_op),
        .rd   (insn_rd),
        .rs   (insn_rs),
        .imm  (insn_imm)
    );

    control u_control (
        .clk     (clk),
        .rst_n   (rst_n),
        .op      (op),
        .zero    (zero),
        .ir_load (ir_load),
        .pc_inc  (pc_inc),
        .pc_load (pc_load),
        .reg_we  (reg_we),
        .out_we  (out_we),
        .halted  (halted)
    );

    datapath u_datapath (
        .clk      (clk),
        .rst_n    (rst_n),
        .insn_op  (insn_op),
        .insn_rd  (insn_rd),
        .insn_rs  (insn_rs),
        .insn_imm (insn_imm),
        .ir_load  (ir_load),
        .pc_inc   (pc_inc),
        .pc_load  (pc_load),
        .reg_we   (reg_we),
        .out_we   (out_we),
        .pc       (pc),
        .op       (op),
        .zero     (zero),
        .out      (out)
    );
endmodule
