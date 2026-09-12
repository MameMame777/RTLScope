// The sequencer: which of the four steps the machine is on, and what each
// step tells the datapath to do.
//
// One instruction takes four clocks — fetch, decode, execute, write back —
// and the state advances on every one of them, so this is a state machine
// in the plainest sense: a register whose next value a `case` on itself
// decides. Written in the two-process style, with the `default:` arm every
// careful machine has, so an encoding the arms do not name goes back to
// FETCH rather than nowhere.
module control
    import cpu_pkg::*;
(
    input  logic    clk,
    input  logic    rst_n,
    input  opcode_t op,
    input  logic    zero,
    output logic    ir_load,
    output logic    pc_inc,
    output logic    pc_load,
    output logic    reg_we,
    output logic    out_we,
    output logic    halted
);
    typedef enum logic [2:0] {
        FETCH,
        DECODE,
        EXECUTE,
        WRITEBACK,
        HALT
    } state_t;

    state_t state, next_state;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            state <= FETCH;
        else
            state <= next_state;
    end

    always_comb begin
        next_state = state;
        case (state)
            FETCH:     next_state = DECODE;
            DECODE:    next_state = EXECUTE;
            EXECUTE:   if (op == HLT) next_state = HALT;
                       else           next_state = WRITEBACK;
            WRITEBACK: next_state = FETCH;
            HALT:      next_state = HALT;
            default:   next_state = FETCH;
        endcase
    end

    // Which instructions leave something in a register. Spelled out rather
    // than derived from the encoding, so adding an opcode means saying here
    // what it writes — which is the question a reader of this file asks.
    logic writes_reg;
    assign writes_reg = (op == LDI) || (op == ADD) || (op == SUB) || (op == AND)
                     || (op == OR)  || (op == XOR) || (op == MOV);

    // Every strobe is a function of the state and the instruction, so a
    // single `case` on the state would say the same thing with the
    // instruction test buried in each arm. Flat is easier to check.
    assign ir_load = (state == FETCH);
    assign pc_inc  = (state == FETCH);
    assign pc_load = (state == EXECUTE) && ((op == JMP) || ((op == JZ) && zero));
    assign reg_we  = (state == WRITEBACK) && writes_reg;
    assign out_we  = (state == WRITEBACK) && (op == OUT);
    assign halted  = (state == HALT);
endmodule
