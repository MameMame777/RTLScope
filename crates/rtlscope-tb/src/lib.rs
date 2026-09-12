//! Generating what it takes to produce a waveform.
//!
//! A dump is the one input RTLScope cannot read out of the source, and this
//! repository had no way to make one: no testbench, no `$dumpvars`, nothing.
//! So the tool writes the harness itself, from what the IR already knows —
//! which ports are clocks, which is the reset and how it asserts, what
//! parameters may be overridden to make a long simulation short.
//!
//! What comes back is text, never files on disk. That keeps the generation
//! testable by snapshot and leaves the decision about where things land, and
//! whether to overwrite anything, with the caller.

pub mod bench;
pub mod clocks;
pub mod cocotb;
pub mod pattern;
pub mod results;
pub mod run;
pub mod sv;
pub mod toolchain;

pub use bench::{Bench, BenchError};
pub use clocks::{ClockPlan, ClockPort, ResetPort, plan};
pub use cocotb::{Generated, Stimulus, TbError, TbOptions, generate};
pub use pattern::{Cell, Lane, Mismatch, Pattern, Verdict};
pub use results::{Outcome, Run, TestOutcome};
pub use run::{Engine, SimError, SimOutcome, Tools, simulate};

/// Which harness to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flavor {
    /// cocotb: stimulus in Python, easier to write and to change.
    #[default]
    Cocotb,
    /// A self-contained `.sv` file, for a checkout with a simulator and no
    /// Python.
    Sv,
}

/// Writes the harness for one module.
pub fn generate_with(
    design: &rtlscope_ir::Design,
    module: rtlscope_ir::ModuleId,
    options: &TbOptions,
    flavor: Flavor,
) -> Result<Generated, TbError> {
    // Before anything is written: a simulator that cannot open the sources
    // fails a minute later, in a message that names a path with `?` in it and
    // blames a missing include directory.
    let unspellable = toolchain::unspellable(&options.sources);
    if !unspellable.is_empty() {
        return Err(TbError::PathOutsideCodePage {
            paths: unspellable,
            code_page: toolchain::code_page(),
        });
    }

    match flavor {
        Flavor::Cocotb => cocotb::generate(design, module, options),
        Flavor::Sv => sv::generate(design, module, options),
    }
}
