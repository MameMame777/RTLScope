// Two roads from the same place to the same place, one clock longer than the
// other.
//
// This is the shape the depth report refuses to average. `sum` is fed from
// `slow` (two registers deep) and from `fast` (one), so a value entering at
// `a` arrives at `sum` partly two clocks later and partly one — which is
// almost always a bug, and almost always this bug: the control path was
// pipelined and the data path was not, or the other way round.
//
// A single number would hide it whichever number was picked. The mean is a
// depth that exists nowhere in the design; the maximum says the shallow path
// is fine; the minimum says the deep one is. So both come back, and the
// disagreement itself is the finding.

module reconverge (
    input  logic       clk,
    input  logic       rst_n,
    input  logic [7:0] a,
    output logic [7:0] sum
);

    logic [7:0] slow_one;
    logic [7:0] slow_two;
    logic [7:0] fast;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            slow_one <= 8'h00;
            slow_two <= 8'h00;
            fast     <= 8'h00;
            sum      <= 8'h00;
        end else begin
            // The long way: two clocks.
            slow_one <= a;
            slow_two <= slow_one;
            // The short way: one.
            fast     <= a;
            // Where they meet, out of step.
            sum      <= slow_two + fast;
        end
    end

endmodule
