//! Printing what a testbench run came to.
//!
//! The interesting column is not pass or fail — the exit code says that — but
//! *when*: a failing test names a moment, and a moment is a place to put the
//! cursor in a waveform. So when a dump is given, each test's moment is
//! converted to that dump's own ticks, and a test that ends past the end of the
//! dump is said to, rather than pointing at nothing.

use std::fmt::Write as _;

use rtlscope_tb::results::{BASIS, Run};
use rtlscope_wave::Dump;

/// What the run did, and where in a dump each test sits.
pub fn results_text(run: &Run, dump: Option<&Dump>, dump_name: &str) -> String {
    let mut out = String::new();
    let (passed, failed, skipped) = run.counts();
    let _ = writeln!(
        out,
        "{} test(s): {passed} passed, {failed} failed, {skipped} skipped",
        run.tests.len()
    );
    let _ = writeln!(out, "{} ns of simulation in all", trim(run.sim_ns()));

    if let Some(dump) = dump {
        match dump.timescale() {
            Some((factor, unit)) => {
                let _ = writeln!(
                    out,
                    "\n{dump_name}: one tick is {factor} {unit}, and it ends at {}",
                    dump.max_time()
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "\n{dump_name} declares no timescale, so nanoseconds cannot be turned into \
                     ticks of it"
                );
            }
        }
    }

    out.push('\n');
    for test in &run.tests {
        let _ = writeln!(
            out,
            "  {:>12} → {:<12}  {:<8} {}",
            trim(test.start_ns),
            trim(test.end_ns),
            test.outcome.label(),
            test.full_name()
        );
        if let Some(message) = &test.message {
            for line in message.lines() {
                let _ = writeln!(out, "        {line}");
            }
        }
        if let Some(file) = &test.file {
            match test.line {
                Some(line) => {
                    let _ = writeln!(out, "        {file}:{line}");
                }
                None => {
                    let _ = writeln!(out, "        {file}");
                }
            }
        }
        if let Some(dump) = dump
            && let Some(tick) = dump.ticks_of_ns(test.end_ns)
        {
            if tick > dump.max_time() {
                let _ = writeln!(
                    out,
                    "        tick {tick} — past the end of {dump_name}, which stops at {}",
                    dump.max_time()
                );
            } else {
                let _ = writeln!(out, "        tick {tick} in {dump_name}");
            }
        }
    }

    if !run.problems.is_empty() {
        let _ = writeln!(out, "\n{} thing(s) in the file did not add up", run.problems.len());
        for problem in &run.problems {
            let _ = writeln!(out, "  {problem}");
        }
    }

    let _ = writeln!(out, "\n{BASIS}");
    out
}

/// A float with no trailing `.0`, since these are whole nanoseconds far more
/// often than not.
fn trim(ns: f64) -> String {
    if ns.fract() == 0.0 && ns.abs() < 1e15 { format!("{}", ns as i64) } else { format!("{ns}") }
}
