// Two-process FSM with localparam state encoding. Exercises case lowering and
// the Comb/Ff split, and is the fixture Phase 5 FSM extraction targets: the
// state register is self-referential through next_state.
module fsm (
    input  logic clk,
    input  logic rst_n,
    input  logic start,
    input  logic done,
    output logic busy
);

    localparam logic [1:0] S_IDLE = 2'd0;
    localparam logic [1:0] S_RUN  = 2'd1;
    localparam logic [1:0] S_WAIT = 2'd2;

    logic [1:0] state, next_state;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            state <= S_IDLE;
        else
            state <= next_state;
    end

    always_comb begin
        next_state = state;
        case (state)
            S_IDLE:  if (start) next_state = S_RUN;
            S_RUN:   next_state = S_WAIT;
            S_WAIT:  if (done)  next_state = S_IDLE;
            default: next_state = S_IDLE;
        endcase
    end

    assign busy = (state != S_IDLE);

endmodule
