/// The sequence: wait for a start, run until the timer ticks, pause for one
/// more tick, then hold Done until the button is let go.
module lights_Control (
    input var logic          i_clk  ,
    input var logic          i_rst_n,
    input var logic          i_start,
    lights_Ticker.user t      ,
    output var logic          o_busy 
);
    typedef enum logic [2-1:0] {
        State_Idle,
        State_Run,
        State_Pause,
        State_Done
    } State;

    State state     ;
    State state_next;

    // The register: one flop, reset to Idle.
    always_ff @ (posedge i_clk, negedge i_rst_n) begin
        if (!i_rst_n) begin
            state <= State_Idle;
        end else begin
            state <= state_next;
        end
    end

    // The next state, from where we are and what came in.
    always_comb begin
        state_next = state;
        case (state)
            State_Idle: begin
                if (i_start) begin
                    state_next = State_Run;
                end
            end
            State_Run: begin
                if (t.tick) begin
                    state_next = State_Pause;
                end
            end
            State_Pause: begin
                if (t.tick) begin
                    state_next = State_Done;
                end
            end
            State_Done: begin
                if (!i_start) begin
                    state_next = State_Idle;
                end
            end
            default: state_next = State_Idle;
        endcase
    end

    always_comb t.enable = state == State_Run || state == State_Pause;
    always_comb o_busy   = state != State_Idle;
endmodule
//# sourceMappingURL=control.sv.map
