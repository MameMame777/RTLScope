// An `always_ff` that declares a variable inside itself — an ordinary idiom
// that used to make the whole process disappear, silently: the lowering
// searched the item for a declaration, found the inner one, and classified the
// block as a net declaration. The module came back with no processes and no
// diagnostic to say why.
module decl_in_process (
    input  logic       clk,
    input  logic       rst_n,
    input  logic [7:0] d,
    output logic [7:0] q
);

    always_ff @(posedge clk) begin
        if (!rst_n) begin
            q <= 8'h00;
        end else begin
            automatic logic [7:0] doubled;
            doubled = d + d;
            q <= doubled;
        end
    end

endmodule
