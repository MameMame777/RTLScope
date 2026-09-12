// Combinational logic that holds a value instead of driving it.
//
// A signal an `always_comb` does not assign on every path through it is not a
// gate — synthesis builds a latch to hold the old value, which is almost never
// what the author meant. Two of these describe latches and two do not, and
// telling them apart is the whole job: a report that flagged `y_ok` too would
// be one nobody reads.
module latch_check (
    input  wire [1:0]  sel,
    input  wire [7:0]  a, b,
    output logic [7:0] y_ok,
    output logic [7:0] y_if,
    output logic [7:0] y_case,
    output logic [7:0] y_default_ok,
    output logic [7:0] y_dead
);
    // Fine: a default assignment before the branch covers every path.
    always_comb begin
        y_ok = 8'h00;
        if (sel == 2'd0) y_ok = a;
    end

    // A latch: nothing assigns it when the condition is false.
    always_comb begin
        if (sel == 2'd0) y_if = a;
        else if (sel == 2'd1) y_if = b;
    end

    // A latch: two of the four values `sel` can take assign nothing.
    always_comb begin
        case (sel)
            2'd0: y_case = a;
            2'd1: y_case = b;
        endcase
    end

    // Fine: every arm, plus a default.
    always_comb begin
        case (sel)
            2'd0:    y_default_ok = a;
            2'd1:    y_default_ok = b;
            default: y_default_ok = 8'hFF;
        endcase
    end

    // Driven, and read by nothing inside this module — but it is a port, so
    // the outside reads it and it is not dead.
    always_comb y_dead = a ^ b;
endmodule

module latch_dead (
    input  wire [7:0] a,
    output logic [7:0] y
);
    // Driven and read by nobody at all: this one is dead.
    logic [7:0] spare;
    always_comb spare = ~a;
    assign y = a;
endmodule
