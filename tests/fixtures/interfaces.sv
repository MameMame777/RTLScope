// A bundle of signals with a name: an interface, its modports, and the three
// ways a module comes by one — declared with a modport, generic, and
// instantiated with its own parameter and port.

interface bus_if #(parameter int W = 8) (input logic clk);
    logic [W-1:0] data;
    logic         valid;
    logic         ready;

    // Logic of its own: the handshake, seen the same from either side.
    logic fire;
    assign fire = valid && ready;

    modport master (input clk, output data, output valid, input ready, input fire);
    modport slave  (input clk, input data, input valid, output ready, input fire);
endinterface

module producer (
    bus_if.master m,
    input  logic  rst_n
);
    always_ff @(posedge m.clk or negedge rst_n) begin
        if (!rst_n) begin
            m.data  <= 8'd0;
            m.valid <= 1'b0;
        end else if (m.fire || !m.valid) begin
            m.data  <= m.data + 8'd1;
            m.valid <= 1'b1;
        end
    end
endmodule

module consumer (
    bus_if.slave s,
    input  logic       rst_n,
    output logic [7:0] last
);
    assign s.ready = rst_n;

    always_ff @(posedge s.clk or negedge rst_n) begin
        if (!rst_n) begin
            last <= 8'd0;
        end else if (s.fire) begin
            last <= s.data;
        end
    end
endmodule

// A generic interface port: whichever interface is connected.
module monitor (
    interface.slave watched,
    output logic    busy
);
    assign busy = watched.valid;
endmodule

module interfaces_top (
    input  logic       clk,
    input  logic       rst_n,
    output logic [7:0] last,
    output logic       busy
);
    bus_if #(.W(8)) bus (.clk(clk));

    producer u_producer (.m(bus), .rst_n(rst_n));
    consumer u_consumer (.s(bus), .rst_n(rst_n), .last(last));
    monitor  u_monitor  (.watched(bus), .busy(busy));
endmodule
