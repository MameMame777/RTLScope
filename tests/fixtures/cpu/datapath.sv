// Where the values live and move: the program counter, the instruction
// register, the register file, the ALU, and the flag and output registers.
//
// The datapath does nothing on its own. Every register here waits on a strobe
// from `control`, and the only decision it makes is the one the ALU makes.
// That split — sequencing in one module, storage and arithmetic in another —
// is the shape almost every processor takes, and the diagram of this module
// shows why: the registers are boxes, the arithmetic is a cloud, and the
// strobes come in from the left as a row of one-bit wires.
module datapath
    import cpu_pkg::*;
(
    input  logic              clk,
    input  logic              rst_n,

    // The instruction the memory is holding out, at `pc`.
    input  opcode_t           insn_op,
    input  logic [1:0]        insn_rd,
    input  logic [1:0]        insn_rs,
    input  logic [DATA_W-1:0] insn_imm,

    // What `control` asks for this clock.
    input  logic              ir_load,
    input  logic              pc_inc,
    input  logic              pc_load,
    input  logic              reg_we,
    input  logic              out_we,

    output logic [PC_W-1:0]   pc,
    output opcode_t           op,
    output logic              zero,
    output logic [DATA_W-1:0] out
);
    // ---- the instruction register: the fetched fields, held for the cycle ----
    logic [1:0]        ir_rd;
    logic [1:0]        ir_rs;
    logic [DATA_W-1:0] ir_imm;

    // ---- the program counter: advances, or is loaded ----
    //
    // A register that reads itself. It is not a pipeline stage — it is state
    // that moves — and the diagram draws the loop from its output back to its
    // input for exactly that reason.
    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            pc <= {PC_W{1'b0}};
        else if (pc_load)
            pc <= ir_imm[PC_W-1:0];
        else if (pc_inc)
            pc <= pc + 1'b1;
    end

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            op     <= NOP;
            ir_rd  <= 2'd0;
            ir_rs  <= 2'd0;
            ir_imm <= {DATA_W{1'b0}};
        end else if (ir_load) begin
            op     <= insn_op;
            ir_rd  <= insn_rd;
            ir_rs  <= insn_rs;
            ir_imm <= insn_imm;
        end
    end

    // ---- registers and arithmetic ----
    logic [DATA_W-1:0] a;
    logic [DATA_W-1:0] b;
    logic [DATA_W-1:0] result;
    logic              result_zero;

    regfile u_regs (
        .clk     (clk),
        .we      (reg_we),
        .waddr   (ir_rd),
        .wdata   (result),
        .raddr_a (ir_rd),
        .raddr_b (ir_rs),
        .rdata_a (a),
        .rdata_b (b)
    );

    alu u_alu (
        .op     (op),
        .a      (a),
        .b      (b),
        .imm    (ir_imm),
        .result (result),
        .zero   (result_zero)
    );

    // ---- the zero flag: what the last written result was ----
    //
    // Remembered at write-back rather than read live, so a `JZ` a few clocks
    // later is asking about the subtraction before it and not about whatever
    // the ALU happens to be showing now.
    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            zero <= 1'b0;
        else if (reg_we)
            zero <= result_zero;
    end

    // ---- the output register: the one thing the outside world sees ----
    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            out <= {DATA_W{1'b0}};
        else if (out_we)
            out <= b;
    end
endmodule
