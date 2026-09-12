// Names shared through packages — constants, an enum, a packed struct and a
// function — reached by `import` and by `pkg::name`.

package defs;
    localparam int WIDTH = 8;
    localparam int DEPTH = 4;

    typedef enum logic [1:0] { OFF, SLOW, FAST } mode_t;

    typedef struct packed {
        logic [WIDTH-1:0] value;
        logic             valid;
    } beat_t;

    // Names a package constant bare; every caller sees WIDTH through it.
    function automatic logic [WIDTH-1:0] double(input logic [WIDTH-1:0] x);
        return x + x;
    endfunction
endpackage

// A package that uses another one.
package more;
    import defs::*;
    localparam int TWICE = WIDTH * 2;
    localparam int LIMIT = defs::DEPTH + 1;
endpackage

// A header import: the parameters and ports may use what it brings in.
module engine
    import defs::*;
#(
    parameter int STAGES = DEPTH
) (
    input  logic             clk,
    input  logic             rst_n,
    input  mode_t            mode,
    input  logic [WIDTH-1:0] data,
    output beat_t            beat,
    output logic             fast
);
    logic [WIDTH-1:0] stage;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            stage <= 8'd0;
            beat  <= 9'd0;
        end else begin
            stage <= data;
            beat  <= {double(stage), 1'b1};
        end
    end

    always_comb begin
        case (mode)
            FAST:    fast = 1'b1;
            default: fast = 1'b0;
        endcase
    end
endmodule

// No import at all: every name is spelled out, and the package function is
// still called with the package's own constants in reach.
module qualified (
    input  logic                   clk,
    input  logic                   rst_n,
    input  defs::mode_t            mode,
    input  logic [more::TWICE-1:0] wide,
    output logic [defs::WIDTH-1:0] out
);
    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            out <= 8'd0;
        end else if (mode == defs::SLOW) begin
            out <= defs::double(wide[defs::WIDTH-1:0]);
        end
    end
endmodule

// One name at a time, and an import inside a generate block.
module picked (
    input  logic                   clk,
    output logic [more::LIMIT-1:0] count
);
    import defs::WIDTH;
    logic [WIDTH-1:0] narrow;

    if (1) begin : g
        import more::TWICE;
        logic [TWICE-1:0] wide;
        always_comb wide = {narrow, narrow};
    end

    always_ff @(posedge clk) begin
        narrow <= narrow + 1'b1;
        count  <= count + 1'b1;
    end
endmodule

// A state machine on an enum from a package.
package walk_pkg;
    typedef enum logic [1:0] { S_IDLE, S_RUN, S_DONE } state_t;
endpackage

module walker
    import walk_pkg::*;
(
    input  logic clk,
    input  logic rst_n,
    input  logic start,
    input  logic done,
    output logic busy
);
    state_t state, state_next;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) state <= S_IDLE;
        else        state <= state_next;
    end

    always_comb begin
        state_next = state;
        case (state)
            S_IDLE:  if (start) state_next = S_RUN;
            S_RUN:   if (done)  state_next = S_DONE;
            S_DONE:  state_next = S_IDLE;
            default: state_next = S_IDLE;
        endcase
    end

    assign busy = state != S_IDLE;
endmodule

module packages_top (
    input  logic        clk,
    input  logic        rst_n,
    input  logic        start,
    input  logic        done,
    input  defs::mode_t mode,
    input  logic [7:0]  data,
    input  logic [15:0] wide,
    output defs::beat_t beat,
    output logic        fast,
    output logic [7:0]  out,
    output logic [4:0]  count,
    output logic        busy
);
    engine    u_engine    (.clk(clk), .rst_n(rst_n), .mode(mode), .data(data), .beat(beat), .fast(fast));
    qualified u_qualified (.clk(clk), .rst_n(rst_n), .mode(mode), .wide(wide), .out(out));
    picked    u_picked    (.clk(clk), .count(count));
    walker    u_walker    (.clk(clk), .rst_n(rst_n), .start(start), .done(done), .busy(busy));
endmodule
