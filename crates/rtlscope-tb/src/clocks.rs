//! Which of a module's inputs are clocks, and which are resets.
//!
//! Nothing in SystemVerilog says so. A port called `clk` is a convention, and
//! conventions are exactly what this tool refuses to guess from — but the IR
//! already knows, because the front end had to decide it to classify a process
//! at all: every `always_ff` names the net it is clocked by and the net that
//! resets it, along with whether that reset is asynchronous and which level
//! asserts it. So a testbench's clock and reset are read out of the design
//! rather than inferred from names.
//!
//! Two things are worth knowing about the result. It looks inside instances,
//! since a wrapper module often has no `always_ff` of its own and its clock is
//! only recognisable as the thing feeding its children. And a port it cannot
//! place is left alone and named in [`ClockPlan::notes`] — a testbench that
//! silently drove a data input as a clock would waste more time than one that
//! says it did not know.

use std::collections::BTreeMap;

use rtlscope_analyse::flat::{self, SignalId};
use rtlscope_ir::{Design, Level, ModuleId, PortDir, ProcKind, ResetKind};

/// How to drive a module for long enough to record something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockPlan {
    pub clocks: Vec<ClockPort>,
    pub resets: Vec<ResetPort>,
    /// Inputs that are neither, which a generated testbench ties off.
    pub tied_off: Vec<String>,
    /// Anything the plan could not work out, in words.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockPort {
    pub port: String,
    /// Nanoseconds. The default is 10; a design with two clocks usually wants
    /// them set apart by hand.
    pub period_ns: u32,
    /// How many flops in the design run on it — the evidence for calling it a
    /// clock, and the reason to believe the busiest one is the main one.
    pub flops: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetPort {
    pub port: String,
    pub active_low: bool,
    pub asynchronous: bool,
    /// How many clock cycles to hold it before releasing.
    pub cycles: u32,
}

/// The default clock period, in nanoseconds.
pub const DEFAULT_PERIOD_NS: u32 = 10;

/// How long a generated testbench holds reset.
pub const DEFAULT_RESET_CYCLES: u32 = 5;

pub fn plan(design: &Design, module_id: ModuleId) -> ClockPlan {
    let flat = flat::flatten(design);
    let module = &design.modules[module_id];

    // Every signal used as a clock or a reset anywhere in the subtree, and how
    // much of the design each drives.
    let mut clock_flops: BTreeMap<SignalId, usize> = BTreeMap::new();
    let mut resets: BTreeMap<SignalId, (Level, ResetKind)> = BTreeMap::new();
    for node in &flat.nodes {
        for process in &design.modules[node.module].procs {
            let ProcKind::Ff { clk, rst, .. } = &process.kind else { continue };
            if let Some(signal) = node.signal_of(clk) {
                *clock_flops.entry(signal).or_default() += 1;
            }
            if let Some(reset) = rst
                && let Some(signal) = node.signal_of(&reset.net)
            {
                resets.insert(signal, (reset.active, reset.kind));
            }
        }
    }

    // The subtree walk starts at the top, so a module below it has its ports
    // bound to the parent's signals. Find this module's own node to read them.
    let own = flat.nodes.iter().find(|node| node.module == module_id).or_else(|| flat.nodes.last());

    let mut plan = ClockPlan {
        clocks: Vec::new(),
        resets: Vec::new(),
        tied_off: Vec::new(),
        notes: Vec::new(),
    };
    let Some(own) = own else {
        plan.notes.push("the design has no modules to drive".into());
        return plan;
    };

    for port in &module.ports {
        if port.dir != PortDir::Input {
            continue;
        }
        let Some(signal) = own.signal(port.net) else {
            plan.tied_off.push(port.name.clone());
            continue;
        };

        if let Some(flops) = clock_flops.get(&signal) {
            plan.clocks.push(ClockPort {
                port: port.name.clone(),
                period_ns: DEFAULT_PERIOD_NS,
                flops: *flops,
            });
        } else if let Some((active, kind)) = resets.get(&signal) {
            plan.resets.push(ResetPort {
                port: port.name.clone(),
                active_low: *active == Level::Low,
                asynchronous: *kind == ResetKind::Async,
                cycles: DEFAULT_RESET_CYCLES,
            });
        } else {
            plan.tied_off.push(port.name.clone());
        }
    }

    // The busiest clock first: a testbench that runs for "a thousand cycles"
    // means the one most of the design is on.
    plan.clocks.sort_by(|a, b| b.flops.cmp(&a.flops).then_with(|| a.port.cmp(&b.port)));

    if plan.clocks.is_empty() {
        plan.notes.push(
            "no input of this module clocks anything, so the testbench has no clock to run \
             — pass --clock NAME=PERIOD to drive one anyway"
                .into(),
        );
    }
    if plan.resets.is_empty() {
        plan.notes
            .push("no input of this module resets anything; nothing will be held at start".into());
    }
    if !plan.tied_off.is_empty() {
        plan.notes.push(format!(
            "{} input(s) are neither clock nor reset and are tied off: {}",
            plan.tied_off.len(),
            plan.tied_off.join(", ")
        ));
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtlscope_sv::ParseOptions;

    fn plan_for(fixture: &str, top: Option<&str>) -> ClockPlan {
        let path = rtlscope_fixtures::path(fixture);
        let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
        let (design, _) = rtlscope_elab::elaborate(&uir, top);
        let design = design.expect("elaboration produced a design");
        plan(&design, design.top)
    }

    #[test]
    fn a_clock_is_found_by_what_it_clocks_not_by_its_name() {
        let plan = plan_for("counter.sv", None);
        assert_eq!(plan.clocks.len(), 1, "{plan:#?}");
        assert_eq!(plan.clocks[0].port, "clk");
        assert!(plan.clocks[0].flops > 0);
    }

    #[test]
    fn a_reset_carries_its_level_and_whether_it_is_asynchronous() {
        // `counter.sv` resets synchronously, active low.
        let plan = plan_for("counter.sv", None);
        assert_eq!(plan.resets.len(), 1, "{plan:#?}");
        assert_eq!(plan.resets[0].port, "rst_n");
        assert!(plan.resets[0].active_low);
        assert!(!plan.resets[0].asynchronous);

        // `pipeline3.sv` resets asynchronously.
        let plan = plan_for("pipeline3.sv", Some("pipeline3"));
        assert_eq!(plan.resets.len(), 1, "{plan:#?}");
        assert!(plan.resets[0].asynchronous, "{plan:#?}");
    }

    #[test]
    fn everything_else_is_tied_off_and_said_so() {
        let plan = plan_for("counter.sv", None);
        assert_eq!(plan.tied_off, ["en"], "{plan:#?}");
        assert!(plan.notes.iter().any(|note| note.contains("tied off")), "{plan:#?}");
    }
}
