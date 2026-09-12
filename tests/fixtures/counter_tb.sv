// A testbench of the kind somebody writes by hand: it drives the design,
// checks it, and records itself. RTLScope reads it for two facts — which
// module to start at, and whether it dumps — and otherwise runs it untouched.
//
// The module has no ports. That is what says it is a testbench, and it needs
// no naming convention to say it.
`timescale 1ns / 1ps

module counter_tb;

    localparam int W = 8;

    logic         clk = 1'b0;
    logic         rst_n = 1'b0;
    logic         en = 1'b0;
    logic [W-1:0] count;

    counter #(.W(W)) dut (
        .clk  (clk),
        .rst_n(rst_n),
        .en   (en),
        .count(count)
    );

    always #5 clk = ~clk;

    // Its own recording, which is the case where RTLScope adds nothing.
    initial begin
        $dumpfile("counter_tb.fst");
        $dumpvars(0, counter_tb);
    end

    int errors = 0;

    task automatic check(input logic [W-1:0] want);
        if (count !== want) begin
            errors = errors + 1;
            $display("[%0t] count is %0d, wanted %0d", $time, count, want);
        end
    endtask

    initial begin
        @(negedge clk);
        rst_n = 1'b0;
        en    = 1'b0;
        repeat (2) @(negedge clk);
        check(8'd0);

        rst_n = 1'b1;
        en    = 1'b1;
        repeat (10) @(negedge clk);
        check(8'd10);

        // Held: the enable is what decides, not the clock.
        en = 1'b0;
        repeat (5) @(negedge clk);
        check(8'd10);

        en = 1'b1;
        repeat (3) @(negedge clk);
        check(8'd13);

        if (errors == 0) $display("counter_tb: PASS");
        else $display("counter_tb: FAIL (%0d)", errors);
        $finish;
    end

endmodule
