//! A second opinion on the IR, from Yosys.
//!
//! Every other view in RTLScope derives from the IR, which means every one of
//! them is wrong in the same way if the IR is. Tests written against the same
//! front end that produced it cannot catch that: they check that RTLScope is
//! consistent, not that it is right. What can catch it is another tool reading
//! the same files — and Yosys is the one that already exists, already reads
//! SystemVerilog, and already writes down what it found.
//!
//! So this reads a Yosys netlist and compares the facts both tools claim to
//! know: which modules there are, what their ports are called and how wide they
//! are, what each parameter evaluated to, and what instantiates what. It never
//! runs Yosys. The netlist is an input like any other, which keeps Yosys an
//! optional dependency and keeps this usable in a CI job that already has one:
//!
//! ```text
//! yosys -p "read_verilog -sv <sources>; hierarchy -top <TOP>; proc; write_json out.json"
//! rtlscope yosys-check out.json <sources> --top <TOP>
//! ```
//!
//! `proc` is not optional: `write_json` refuses a module that still contains
//! processes. It turns `always` blocks into cells, which changes nothing this
//! compares.

pub mod compare;
pub mod netlist;

pub use compare::{CrossCheck, Difference, Kind, ModuleCheck, check};
pub use netlist::{Netlist, NetlistError, ParamValue, read};
