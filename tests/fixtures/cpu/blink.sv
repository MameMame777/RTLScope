// The other design in the folder.
//
// A project folder rarely holds exactly one thing, and this one holds two:
// the CPU, and this — a counter whose top bit is an LED. Nothing instantiates
// either, so a reader who opens the folder is asked which one they meant,
// which is the question a real project folder asks too.
module blink #(
    parameter int PERIOD = 12
) (
    input  logic clk,
    input  logic rst_n,
    output logic led
);
    logic [PERIOD-1:0] count;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            count <= {PERIOD{1'b0}};
        else
            count <= count + 1'b1;
    end

    assign led = count[PERIOD-1];
endmodule
