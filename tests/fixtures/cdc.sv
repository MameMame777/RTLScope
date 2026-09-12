// Two clocks, and three ways of getting a signal from one to the other.
//
// `flag` goes through a two-flop synchroniser, which is the one idiom the
// analysis recognises. `count` crosses bare and several bits at once, which is
// the case worth finding. `handshake` crosses through an idiom that is
// perfectly correct and is still reported, because a report that quietly
// approved of things it had not checked would be worth nothing.
module cdc_child (
    input  wire        dst_clk,
    input  wire        dst_rst_n,
    input  wire        flag_src,
    input  wire [7:0]  count_src,
    output logic       flag_dst,
    output logic [7:0] count_dst
);
    logic flag_sync1, flag_sync2;

    always_ff @(posedge dst_clk or negedge dst_rst_n) begin
        if (!dst_rst_n) begin
            flag_sync1 <= 1'b0;
            flag_sync2 <= 1'b0;
            count_dst  <= 8'h00;
        end else begin
            // The synchroniser lives in the same block as everything else,
            // which is how it is usually written.
            flag_sync1 <= flag_src;
            flag_sync2 <= flag_sync1;
            count_dst  <= count_src;
        end
    end

    assign flag_dst = flag_sync2;
endmodule

module cdc_top (
    input  wire        src_clk,
    input  wire        dst_clk,
    input  wire        rst_n,
    input  wire        pulse,
    output logic       flag_out,
    output logic [7:0] count_out
);
    logic       flag_src;
    logic [7:0] count_src;

    always_ff @(posedge src_clk or negedge rst_n) begin
        if (!rst_n) begin
            flag_src  <= 1'b0;
            count_src <= 8'h00;
        end else begin
            flag_src  <= pulse;
            count_src <= count_src + 8'd1;
        end
    end

    cdc_child u_child (
        .dst_clk   (dst_clk),
        .dst_rst_n (rst_n),
        .flag_src  (flag_src),
        .count_src (count_src),
        .flag_dst  (flag_out),
        .count_dst (count_out)
    );
endmodule
