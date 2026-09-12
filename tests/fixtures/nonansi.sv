// Non-ANSI header: the port list is bare names and the directions arrive as
// body declarations. Vendor templates and older IP still look like this, so
// Zybo RTL will hit it (Step 7).
module nonansi (clk, rst_n, d, q);

    input        clk;
    input        rst_n;
    input  [7:0] d;
    output [7:0] q;

    reg [7:0] q;

    always @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            q <= 8'h00;
        else
            q <= d;
    end

endmodule
