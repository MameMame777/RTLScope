// A design built around one question: how many clocks from here to there?
//
// Four roads leave `sample_in`, and each one is a different answer the depth
// analysis can give. They are in one module on purpose — this is the shape a
// real block has, and the point is that the four are indistinguishable by
// eye and different the moment you count.
//
//   sample_in -> sample_out   3 clocks, one road, nothing to qualify it
//   sample_in -> stamped      3 or 4 — the bug: the qualifier is a clock
//                             short of the data it qualifies
//   sample_in -> captured     4 clocks, but only on the cycles `take` allows
//   sample_in -> total        at least 4, and no most: the accumulator
//                             feeds itself
//   valid_in  -> cfg_busy     no answer: it crosses to another clock
//
// The second is the one worth the tool. `present` is derived from the data and
// escorted alongside it, but through two registers where the data goes through
// three — so `stamped` qualifies each sample with the presence flag of the one
// behind it. Nothing about either line looks wrong; the two are wrong together,
// and a reader counting registers by eye has to hold both chains in their head
// at once to see it.

module depth_demo (
    input  logic        clk,
    input  logic        rst_n,
    // The configuration side runs on its own clock, which is what makes the
    // last road unanswerable rather than long.
    input  logic        cfg_clk,
    input  logic        cfg_rst_n,

    input  logic [15:0] sample_in,
    input  logic        valid_in,
    // The enable. A register behind one still advances one stage; it simply
    // does not do it every cycle.
    input  logic        take,

    output logic [15:0] sample_out,
    output logic        valid_out,
    output logic [15:0] stamped,
    output logic [15:0] captured,
    output logic [15:0] total,
    output logic        cfg_busy
);

    // ---- the data: three clocks, plainly ----
    logic [15:0] stage1, stage2, stage3;

    // ---- the escort: two clocks, which is one too few ----
    logic present;
    logic present_d1, present_d2;

    // ---- the qualified output, where the two meet ----
    // (declared above as a port)

    // ---- the gated capture and the accumulator ----
    logic [15:0] held;
    logic [15:0] running;

    // ---- the control chain, and the flag that leaves this clock ----
    logic valid_d1, valid_d2;
    logic flag;

    assign present    = |sample_in;
    assign sample_out = stage3;
    assign valid_out  = valid_d2;
    assign captured   = held;
    assign total      = running;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            stage1     <= 16'h0000;
            stage2     <= 16'h0000;
            stage3     <= 16'h0000;
            present_d1 <= 1'b0;
            present_d2 <= 1'b0;
            stamped    <= 16'h0000;
            valid_d1   <= 1'b0;
            valid_d2   <= 1'b0;
            flag       <= 1'b0;
            held       <= 16'h0000;
            running    <= 16'h0000;
        end else begin
            // Three deep.
            stage1 <= sample_in;
            stage2 <= stage1 + 16'd1;
            stage3 <= stage2;

            // Two deep, escorting three deep. This is the bug.
            present_d1 <= present;
            present_d2 <= present_d1;
            stamped    <= present_d2 ? stage3 : 16'h0000;

            // The control chain, and the flag that crosses.
            valid_d1 <= valid_in;
            valid_d2 <= valid_d1;
            flag     <= valid_d2;

            // One stage, taken only when `take` says so.
            if (take) begin
                held <= stage3;
            end

            // One stage that feeds itself: there is no longest road through it.
            if (valid_d2) begin
                running <= running + stage3;
            end
        end
    end

    // ---- the other clock ----
    logic cfg_sync1, cfg_sync2;

    assign cfg_busy = cfg_sync2;

    always_ff @(posedge cfg_clk or negedge cfg_rst_n) begin
        if (!cfg_rst_n) begin
            cfg_sync1 <= 1'b0;
            cfg_sync2 <= 1'b0;
        end else begin
            cfg_sync1 <= flag;
            cfg_sync2 <= cfg_sync1;
        end
    end

endmodule
