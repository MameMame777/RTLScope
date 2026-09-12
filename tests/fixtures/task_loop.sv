// Constructs whose hardware equivalent is not obvious from the syntax.
//
// Each of these was skipped at one point on the grounds of "hardware has no
// such thing", and each turned out to have a perfectly ordinary equivalent:
// a task is a function called for what it writes back, a loop bounded by a
// signal is that many guarded copies, `initial` is what the design powers up
// holding, and an early `return` is a flag saying the answer is already
// settled.
module task_loop (
    input  wire        clk,
    input  wire [1:0]  addr,
    input  wire [7:0]  value,
    input  wire [2:0]  amount,
    output logic [7:0] picked,
    output logic [7:0] rotated,
    output logic [3:0] rated,
    output logic [7:0] rom_data,
    output logic [3:0] counted
);
    // ---- a task: no result, results handed back through `output` ----------
    task automatic pick(input logic [1:0] idx, input logic [7:0] v,
                        output logic [7:0] byte_out);
        case (idx)
            2'd0:    byte_out = 8'hA5;
            2'd1:    byte_out = v;
            default: byte_out = 8'h00;
        endcase
    endtask

    // ---- a loop whose trip count is a signal ------------------------------
    // `n` is three bits, so eight copies guarded by `idx < n` are exactly the
    // barrel shifter this describes — no approximation involved.
    function automatic logic [7:0] rotate_right8(input logic [7:0] v, input logic [2:0] n);
        logic [7:0] result;
        result = v;
        for (int idx = 0; idx < n; idx++) begin
            result = {result[0], result[7:1]};
        end
        return result;
    endfunction

    // ---- an early `return` -----------------------------------------------
    function automatic logic [3:0] rate(input logic [7:0] d);
        logic [3:0] score;
        score = 4'd0;
        if (d == 8'h00) begin
            score = 4'd15;
            return score;
        end
        if (d[7]) begin
            score = 4'd9;
            return score;
        end
        score = 4'd1;
        return score;
    endfunction

    // ---- `x++` as a statement --------------------------------------------
    function automatic logic [3:0] ones4(input logic [3:0] bits);
        ones4 = 0;
        for (int i = 0; i < 4; i++) begin
            if (bits[i]) begin
                ones4++;
            end
        end
    endfunction

    // ---- `initial`: what the memory powers up holding ---------------------
    logic [7:0] rom [0:3];
    initial begin
        rom[0] = 8'h11;
        rom[1] = 8'h22;
        rom[2] = 8'h33;
        rom[3] = 8'h44;
    end

    always_comb begin
        pick(addr, value, picked);
    end

    assign rotated = rotate_right8(value, amount);
    assign rated   = rate(value);
    assign counted = ones4(value[3:0]);

    always_ff @(posedge clk) rom_data <= rom[addr];
endmodule
