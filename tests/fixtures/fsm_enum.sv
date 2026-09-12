// The same FSM written with a typedef enum, which is how real RTL writes one.
// The typedef gives `state` its width and makes S_IDLE and friends constants;
// without it they read as undeclared signals and the design grows a phantom
// one-bit net per state name.
module fsm_enum (
    input  logic clk,
    input  logic rst_n,
    input  logic start,
    output logic busy
);

    typedef enum logic [1:0] {
        S_IDLE = 2'd0,
        S_RUN  = 2'd1,
        S_DONE = 2'd2
    } state_e;

    state_e state, next_state;

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
            S_RUN:   next_state = S_DONE;
            S_DONE:  next_state = S_IDLE;
            default: next_state = S_IDLE;
        endcase
    end

    assign busy = (state != S_IDLE);

endmodule
