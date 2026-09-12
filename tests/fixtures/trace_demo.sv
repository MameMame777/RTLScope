// A design built around one question: why is `out_data` the value it is?
//
// The waveform shows that it changes on some beats and not others, which is
// what a waveform can say. Why is provenance's job, and the path from
// `out_data` back to the edge of the design walks through every kind of answer
// the Trace view gives, in order:
//
//   out_data   assign                  combinational
//   staged     always_ff, guarded      register on clk, reset rst_n
//                                      — `in_data` is data, `gate` is a condition
//   gate       u_gate.allow            from an instance   -> go inside
//   allow      always_ff with a case   register, `case (mode)` and `if (want)`
//   want       a port of the child     from outside       -> go up
//   in_valid   a port of the top       the edge of the design
//
// `mode` decides whether the gate passes every beat, exactly one, or none, so
// the recording has stretches where data is held for a reason that is nowhere
// in `trace_demo` itself — it is two hops away, inside the child.

module trace_gate (
    input  logic       clk,
    input  logic       rst_n,
    input  logic       want,
    input  logic [1:0] mode,
    output logic       allow
);

    localparam logic [1:0] HOLD = 2'd0;
    localparam logic [1:0] PASS = 2'd1;
    localparam logic [1:0] ONCE = 2'd2;

    // Set once `ONCE` has let a beat through, so the second one is refused.
    logic used;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            allow <= 1'b0;
            used  <= 1'b0;
        end else begin
            case (mode)
                PASS: allow <= want;
                ONCE: begin
                    allow <= want && !used;
                    if (want) used <= 1'b1;
                end
                default: allow <= 1'b0;
            endcase
        end
    end

endmodule

module trace_demo (
    input  logic       clk,
    input  logic       rst_n,
    input  logic [7:0] in_data,
    input  logic       in_valid,
    input  logic [1:0] mode,
    output logic [7:0] out_data,
    output logic       out_valid
);

    logic [7:0] staged;
    logic       armed;
    logic       gate;

    trace_gate u_gate (
        .clk   (clk),
        .rst_n (rst_n),
        .want  (in_valid),
        .mode  (mode),
        .allow (gate)
    );

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            staged <= 8'h00;
            armed  <= 1'b0;
        end else if (gate) begin
            staged <= in_data;
            armed  <= 1'b1;
        end
    end

    assign out_data  = staged;
    assign out_valid = armed && gate;

endmodule
