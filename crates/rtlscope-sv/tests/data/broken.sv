// Deliberately invalid: the assignment has no right-hand side. Used to check
// that a parse failure is reported at the offending line rather than as a raw
// byte offset. Not in tests/fixtures/, which iverilog is expected to accept.
module broken (input logic a);
  assign a = ;
endmodule
