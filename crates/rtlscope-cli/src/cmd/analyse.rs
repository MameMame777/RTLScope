//! Printing what the two analyses found.
//!
//! Both have a JSON form for tools and a text form for reading. The text form
//! is the one that gets looked at, so it leads with what a person wants to know
//! — how many machines, which clocks, what crosses between them — and keeps the
//! detail underneath.

use rtlscope_analyse::cone::{Cone, Towards};
use rtlscope_analyse::{
    Crossing, CrossingKind, DepthError, DepthReport, DepthWarning, DomainReport, Fsm, LintReport,
    PipelineReport,
};
use rtlscope_ir::FileTable;

/// The state machines, as text.
pub fn fsms_text(fsms: &[Fsm], files: &FileTable) -> String {
    let mut out = String::new();
    if fsms.is_empty() {
        out.push_str("no state machines found\n");
        out.push_str(
            "\nA state machine here means a register whose next value a `case` on itself \
             decides.\nA design that encodes state some other way will not show up.\n",
        );
        return out;
    }

    out.push_str(&format!("{} state machine(s)\n", fsms.len()));
    for fsm in fsms {
        out.push_str(&format!(
            "\n{}.{}  ({} states, {} transitions)\n",
            fsm.module_name,
            fsm.state_name,
            fsm.states.len(),
            fsm.transitions.len()
        ));
        out.push_str(&format!("  at      {}\n", files.render(fsm.span)));
        out.push_str(&format!("  clock   {}\n", fsm.clock));
        if let Some(next) = &fsm.next_name {
            out.push_str(&format!("  next    {next}\n"));
        }
        if let Some(reset) = &fsm.reset_state {
            out.push_str(&format!("  reset   {reset}\n"));
        }

        out.push_str("  transitions\n");
        for transition in &fsm.transitions {
            let guard = if transition.guard.is_empty() {
                String::new()
            } else {
                format!("  when {}", transition.guard.join(" && "))
            };
            out.push_str(&format!("    {} -> {}{}\n", transition.from, transition.to, guard));
        }

        if !fsm.unreachable.is_empty() {
            out.push_str(&format!("  never entered   {}\n", fsm.unreachable.join(", ")));
        }
        if !fsm.terminal.is_empty() {
            out.push_str(&format!("  never left      {}\n", fsm.terminal.join(", ")));
        }
    }
    out
}

/// The clock domains and what crosses between them, as text.
pub fn cdc_text(report: &DomainReport, files: &FileTable) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} clock domain(s) over {} clocked process(es)\n",
        report.domains.len(),
        report.flops
    ));
    for domain in &report.domains {
        out.push_str(&format!("  {:>5}  {}\n", domain.flops, domain.clock));
    }

    let handled = report.crossings.iter().filter(|c| c.kind.is_handled()).count();
    let bare = report.crossings.len() - handled;
    out.push_str(&format!(
        "\n{} crossing(s): {handled} into a two-flop synchroniser, {bare} not recognised\n",
        report.crossings.len()
    ));

    if report.crossings.is_empty() {
        return out;
    }

    // The ones that need looking at first.
    for group in [false, true] {
        let mut header = false;
        for crossing in report.crossings.iter().filter(|c| c.kind.is_handled() == group) {
            if !header {
                out.push_str(if group {
                    "\nsynchronised\n"
                } else {
                    "\nno synchroniser recognised\n"
                });
                header = true;
            }
            out.push_str(&describe(crossing, files));
        }
    }

    if bare > 0 {
        out.push_str(
            "\nOnly the two-flop synchroniser is recognised. An async FIFO, a gray-coded\n\
             pointer or a handshake is correct and will still be listed above: the list is\n\
             what to check, not what is wrong.\n",
        );
    }
    out
}

fn describe(crossing: &Crossing, files: &FileTable) -> String {
    let kind = match crossing.kind {
        CrossingKind::TwoFlopSynchroniser => "2-flop",
        CrossingKind::Unsynchronised => "1 bit",
        CrossingKind::MultiBitUnsynchronised => "multi-bit",
    };
    format!(
        "  {} -> {}  {} [{} bit(s), {}]\n    in {} at {}\n",
        crossing.from,
        crossing.to,
        crossing.signal,
        crossing.width,
        kind,
        crossing.at,
        files.render(crossing.span)
    )
}

/// The inferred latches and dead signals, as text.
pub fn lint_text(report: &LintReport, files: &FileTable) -> String {
    let mut out = String::new();
    if report.is_empty() {
        return "nothing to report: no inferred latches, no dead signals, no combinational loops\n"
            .to_string();
    }

    // First, because it is the only one of the three that will not synthesise.
    if !report.comb_loops.is_empty() {
        out.push_str(&format!("{} combinational loop(s)\n", report.comb_loops.len()));
        out.push_str(
            "  A value that reaches itself without passing a clock has no settled\n  answer: synthesis refuses it, and simulation reports whichever order\n  it happened to evaluate in.\n\n",
        );
        for found in &report.comb_loops {
            out.push_str(&format!("  {}\n", found.signals.join(" -> ")));
            for (name, span) in found.signals.iter().zip(&found.spans) {
                out.push_str(&format!("    {name} at {}\n", files.render(*span)));
            }
            if found.partial {
                out.push_str(
                    "    one of these is written a slice at a time; RTLScope counts whole\n    signals, so this loop may not be one\n",
                );
            }
        }
        out.push('\n');
    }

    if !report.latches.is_empty() {
        out.push_str(&format!("{} inferred latch(es)\n", report.latches.len()));
        out.push_str(
            "  Combinational logic that does not assign a signal on every path holds it\n  \
             instead, which is a latch whether or not one was wanted.\n\n",
        );
        for latch in &report.latches {
            out.push_str(&format!("  {}.{}\n", latch.module, latch.net));
            out.push_str(&format!("    {}\n", latch.because));
            out.push_str(&format!("    at {}\n", files.render(latch.span)));
        }
    }

    if !report.dead_nets.is_empty() {
        out.push_str(&format!("\n{} signal(s) driven and never read\n", report.dead_nets.len()));
        for net in &report.dead_nets {
            out.push_str(&format!(
                "  {}.{} [{} bit(s)] at {}\n",
                net.module,
                net.net,
                net.width,
                files.render(net.span)
            ));
        }
    }

    if !report.dead_nets.is_empty() {
        out.push_str(
            "
  A cluster of these usually means a feature is compiled out:
",
        );
        out.push_str(
            "  signals a `ifdef region would have read are driven and then
",
        );
        out.push_str(
            "  read by nobody. Pass the defines the build uses (-D NAME) to see
",
        );
        out.push_str(
            "  the configuration you ship.
",
        );
    }

    if !report.dead_modules.is_empty() {
        out.push_str(&format!(
            "\n{} module(s) elaborated and instantiated by nothing\n",
            report.dead_modules.len()
        ));
        for module in &report.dead_modules {
            out.push_str(&format!("  {module}\n"));
        }
    }
    out
}

/// How deep the logic is, as text.
pub fn pipeline_text(report: &PipelineReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} register(s) over {} clock domain(s)\n",
        report.registers,
        report.domains.len()
    ));

    for domain in &report.domains {
        out.push_str(&format!("\n{}  — {} stage(s) deep\n", domain.clock, domain.depth));
        for stage in &domain.stages {
            let names = if stage.registers.len() > 8 {
                format!(
                    "{}, ... ({} in all)",
                    stage.registers[..8].join(", "),
                    stage.registers.len()
                )
            } else {
                stage.registers.join(", ")
            };
            out.push_str(&format!("  stage {:>3}  {names}\n", stage.index));
        }
        if !domain.feedback.is_empty() {
            out.push_str(&format!(
                "  {} group(s) of registers that feed each other — counters, accumulators,\n  \
                 state machines. They advance together, so they share a stage.\n",
                domain.feedback.len()
            ));
            for group in domain.feedback.iter().take(8) {
                let names = if group.registers.len() > 6 {
                    format!(
                        "{}, ... ({} in all)",
                        group.registers[..6].join(", "),
                        group.registers.len()
                    )
                } else {
                    group.registers.join(", ")
                };
                out.push_str(&format!("    at stage {:>3}  {names}\n", group.stage));
            }
        }
    }
    out
}

/// How many clocks apart two signals are, in words.
pub fn depth_text(report: &DepthReport, files: &FileTable) -> String {
    let mut out = String::new();

    if !report.errors.is_empty() {
        for error in &report.errors {
            match error {
                DepthError::CrossDomain { clocks, registers } => {
                    out.push_str(&format!(
                        "E001 `{}` and `{}` are not on one clock\n",
                        report.from, report.to
                    ));
                    out.push_str(&format!("  the road passes {}\n", clocks.join(" and ")));
                    for name in registers {
                        out.push_str(&format!("    at {name}\n"));
                    }
                    out.push_str(
                        "  \"how many clocks\" has no answer across a crossing; `rtlscope cdc`\n",
                    );
                    out.push_str("  reports the crossing itself\n");
                }
                DepthError::CombLoop { signals } => {
                    out.push_str(&format!(
                        "E002 a value on the road decides itself: {}\n",
                        signals.join(" -> ")
                    ));
                    out.push_str(
                        "  it never settles, so counting the clocks it takes counts\n  \
something that does not happen. `rtlscope lint` reports the loop\n",
                    );
                }
                DepthError::NoPath { from, to } => {
                    out.push_str(&format!("E003 nothing that leaves `{from}` arrives at `{to}`\n"));
                }
                DepthError::UnknownSignal { name, candidates } => {
                    out.push_str(&format!("E004 no signal `{name}`\n"));
                    if !candidates.is_empty() {
                        out.push_str("  did you mean:\n");
                        for candidate in candidates {
                            out.push_str(&format!("    {candidate}\n"));
                        }
                    }
                }
            }
        }
        return out;
    }

    let clock = match &report.clock {
        Some(clock) => format!(" on {clock}"),
        None => String::new(),
    };
    let count = match (report.min_stages, report.max_stages) {
        (Some(min), Some(max)) if min == max => format!("{min} clock(s)"),
        (Some(min), Some(max)) => format!("{min} to {max} clock(s)"),
        (Some(min), None) => format!("at least {min} clock(s)"),
        _ => "no answer".to_string(),
    };
    out.push_str(&format!("{} -> {}: {count}{clock}\n", report.from, report.to));

    if report.feedback {
        out.push_str(
            "\n  Something on the way holds itself up — a counter, a state machine,\n  \
a register that shifts into itself — so the walk can go round again and\n  \
there is no longest path. What it takes in practice is a question for a\n  \
dump, not for the structure.\n",
        );
    }

    for problem in &report.problems {
        out.push_str(&format!("  note: {problem}\n"));
    }

    for warning in &report.warnings {
        match warning {
            DepthWarning::Reconvergent { min, max } => {
                out.push_str(&format!(
                    "\nW001 the value arrives {min} clock(s) one way and {max} the other\n"
                ));
                out.push_str(
                    "  Almost always one branch was pipelined and the other was not, and\n  \
the control signal no longer lines up with the data it escorts. Both\n  \
numbers are given because the average is a depth that is nowhere in\n  \
the design.\n",
                );
            }
            DepthWarning::Gated { registers, guards } => {
                out.push_str(&format!(
                    "\nW002 {} register(s) do not advance every cycle\n",
                    registers.len()
                ));
                for name in registers {
                    out.push_str(&format!("  {name}\n"));
                }
                for guard in guards {
                    out.push_str(&format!("    under {guard}\n"));
                }
                out.push_str("  The structure is still this deep; the timing is not fixed.\n");
            }
            DepthWarning::Truncated { shown, cap } => {
                out.push_str(&format!(
                    "\nW004 more ways through than the {cap} shown; {shown} listed\n"
                ));
            }
        }
    }

    if !report.paths.is_empty() {
        out.push_str(&format!("\n{} way(s) through\n", report.paths.len()));
        for path in &report.paths {
            if path.registers.is_empty() {
                out.push_str("  0: one clock's worth of logic, no register between\n");
                continue;
            }
            out.push_str(&format!("  {}:\n", path.stages));
            for register in &path.registers {
                let gate = match path.gated_at.contains(&register.name) {
                    true => "  (gated)",
                    false => "",
                };
                out.push_str(&format!(
                    "    {} at {}{gate}\n",
                    register.name,
                    files.render(register.span)
                ));
            }
        }
    }

    out
}

/// The signal a name refers to, however it was written.
///
/// A full hierarchical path or the bare tail of one: a reader asking about
/// `counter` should not have to type `tb.dut.u_core.counter` first, and a
/// design where two modules both have a `counter` is exactly when they should.
pub fn signal_named(
    flat: &rtlscope_analyse::flat::Flattened,
    design: &rtlscope_ir::Design,
    want: &str,
) -> Option<rtlscope_analyse::flat::SignalId> {
    let mut exact = None;
    let mut tail = None;
    for (name, signal, ..) in flat.all_names(design) {
        if name == want {
            exact = Some(signal);
            break;
        }
        if tail.is_none() && name.ends_with(&format!(".{want}")) {
            tail = Some(signal);
        }
    }
    exact.or(tail)
}

/// A cone, as levels of names.
///
/// Levels rather than a tree, because a cone is not one: two paths reconverge
/// on the same signal all the time, and drawing it as a tree would either
/// repeat that signal or hide the reconvergence — which is usually the thing
/// worth seeing.
pub fn cone_text(cone: &Cone, flat: &rtlscope_analyse::flat::Flattened) -> String {
    let mut out = String::new();
    let heading = match cone.towards {
        Towards::Drivers => "what decides",
        Towards::Loads => "what is decided by",
    };
    out.push_str(&format!("{heading} `{}`\n", flat.name_of(cone.root)));

    for level in 1..=cone.depth() {
        let mut names: Vec<String> = cone
            .level(level)
            .map(|node| {
                let mark = if node.frontier { "  (across a register)" } else { "" };
                format!("    {}{mark}", flat.name_of(node.signal))
            })
            .collect();
        if names.is_empty() {
            continue;
        }
        names.sort();
        out.push_str(&format!("  {level} hop(s) away:\n"));
        for name in names {
            out.push_str(&name);
            out.push('\n');
        }
    }

    if cone.nodes.len() == 1 {
        out.push_str("  nothing — this signal is at the edge of the design\n");
    }
    if cone.clipped > 0 {
        out.push_str(&format!(
            "  {} more left out: a level wider than {} is not a picture\n",
            cone.clipped,
            rtlscope_analyse::cone::MAX_PER_LEVEL
        ));
    }
    out
}

/// The same, as JSON, with the names resolved.
///
/// `SignalId` is an index into a flattening that only exists inside one run,
/// so handing it to a caller would be handing them a number that means nothing
/// tomorrow.
pub fn cone_json(cone: &Cone, flat: &rtlscope_analyse::flat::Flattened) -> serde_json::Value {
    let node = |node: &rtlscope_analyse::cone::ConeNode| {
        serde_json::json!({
            "signal": flat.name_of(node.signal),
            "level": node.level,
            "across_a_register": node.frontier,
        })
    };
    serde_json::json!({
        "root": flat.name_of(cone.root),
        "towards": match cone.towards {
            Towards::Drivers => "drivers",
            Towards::Loads => "loads",
        },
        "nodes": cone.nodes.iter().map(node).collect::<Vec<_>>(),
        "edges": cone
            .edges
            .iter()
            .map(|(from, to, kind)| {
                serde_json::json!({
                    "from": flat.name_of(*from),
                    "to": flat.name_of(*to),
                    "through": match kind {
                        rtlscope_analyse::cone::EdgeKind::Clocked => "a register",
                        rtlscope_analyse::cone::EdgeKind::Comb => "logic",
                    },
                })
            })
            .collect::<Vec<_>>(),
        "clipped": cone.clipped,
    })
}
