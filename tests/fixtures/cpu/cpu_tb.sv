// The CPU's testbench, of the kind somebody writes by hand: it clocks the
// processor out of reset, watches what the program shows on `out`, and says
// whether that was the right sequence. RTLScope reads it for two facts — it is
// the module with no ports, and it dumps for itself — and runs it untouched.
//
// The program in `imem.sv` counts down from five and halts, showing each
// value on the way. So the check is the sequence 5, 4, 3, 2, 1 — one `out`
// change per OUT instruction — followed by `halted`. Nothing here knows how
// many clocks an instruction takes; a program that reached the same values
// by a different schedule would pass, which is the right amount to demand of
// a design whose timing is its own business.
`timescale 1ns / 1ps

module cpu_tb;

    logic       clk = 1'b0;
    logic       rst_n = 1'b0;
    logic [7:0] out;
    logic       halted;

    cpu dut (
        .clk   (clk),
        .rst_n (rst_n),
        .out   (out),
        .halted(halted)
    );

    always #5 clk = ~clk;

    // Its own recording, which is the case where RTLScope adds nothing.
    initial begin
        $dumpfile("cpu_tb.fst");
        $dumpvars(0, cpu_tb);
    end

    // What the program shows, in order.
    localparam int SHOWN = 5;
    logic [7:0] want [0:SHOWN-1] = '{8'd5, 8'd4, 8'd3, 8'd2, 8'd1};

    int         seen = 0;
    int         errors = 0;
    logic [7:0] last_out = 8'd0;

    // Every change of `out` is one OUT instruction landing. Sampled on the
    // falling edge, when a register written on the rising one has settled.
    always @(negedge clk) begin
        if (rst_n && out !== last_out) begin
            if (seen < SHOWN && out !== want[seen]) begin
                errors = errors + 1;
                $display("[%0t] out is %0d, wanted %0d", $time, out, want[seen]);
            end
            seen     = seen + 1;
            last_out = out;
        end
    end

    initial begin
        repeat (2) @(negedge clk);
        rst_n = 1'b1;

        // Until the program halts, with a ceiling so a machine that never
        // halts is a failure rather than a simulation that never ends. Four
        // clocks an instruction and two dozen instructions is under a hundred;
        // four hundred is room for any reasonable change to the program.
        repeat (400) begin
            @(negedge clk);
            if (halted) break;
        end

        if (!halted) begin
            errors = errors + 1;
            $display("[%0t] the program did not halt", $time);
        end
        if (seen != SHOWN) begin
            errors = errors + 1;
            $display("[%0t] out changed %0d time(s), wanted %0d", $time, seen, SHOWN);
        end

        if (errors == 0) $display("cpu_tb: PASS");
        else $display("cpu_tb: FAIL (%0d)", errors);
        $finish;
    end

endmodule
