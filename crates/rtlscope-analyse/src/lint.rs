//! Two things the IR can be asked that the compiler will not tell you.
//!
//! **Inferred latches.** Combinational logic that does not assign a signal on
//! every path through it does not describe a gate — it describes storage, and
//! synthesis builds a latch to hold the old value. Almost always that is a
//! missing `else` or a `case` with no `default` rather than anything the author
//! meant. The IR knows which processes are combinational and what each path
//! through them assigns, so the question is answerable exactly.
//!
//! **Dead signals.** A net that is driven and never read is either a leftover
//! or a connection someone forgot to make. Which of the two it is needs a
//! person; finding them does not.
//!
//! Both err towards saying nothing. A process with a construct the front end
//! could not model is skipped rather than guessed at, because the guess would
//! be a latch report on logic that is fine — and a report that cries wolf is
//! one nobody reads.

use std::collections::BTreeSet;

use rtlscope_ir::{
    Conn, Design, Module, ModuleId, NetId, NetKind, PortDir, ProcKind, Span, Stmt, StmtKind,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintReport {
    pub latches: Vec<Latch>,
    pub dead_nets: Vec<DeadNet>,
    /// Modules elaborated but instantiated by nothing, other than the top.
    pub dead_modules: Vec<String>,
    /// Values that reach themselves without passing a clock.
    ///
    /// Added after the other three, so it carries the attribute that keeps a
    /// report with none of them serialising exactly as it did before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comb_loops: Vec<CombLoop>,
}

impl LintReport {
    pub fn is_empty(&self) -> bool {
        self.latches.is_empty()
            && self.dead_nets.is_empty()
            && self.dead_modules.is_empty()
            && self.comb_loops.is_empty()
    }
}

/// A signal combinational logic holds rather than drives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latch {
    pub module: String,
    pub net: String,
    /// Which path through the process leaves it unassigned.
    pub because: String,
    /// Where the gap is, rather than where the process starts.
    pub span: Span,
    pub process_span: Span,
}

/// A cycle of wires: a value that decides itself.
///
/// Synthesis cannot build one, and simulation answers with whichever order it
/// happened to evaluate in — so this is a bug rather than a style, and the two
/// assignments that make it are each innocent on their own. That is why it is
/// worth a report: it is invisible line by line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CombLoop {
    /// The signals in the cycle, by the name the design knows them by.
    pub signals: Vec<String>,
    /// Where each of them is declared, in the same order.
    pub spans: Vec<Span>,
    /// The cycle may be an artefact of counting whole nets.
    ///
    /// RTLScope tracks a signal, not its bits: `a[1] = b[0]; b[1] = a[0];` has no
    /// loop in it, and this reports one. The flag is set when any member is
    /// written through a slice, which is when that doubt applies — see the note
    /// on grain in `crate::drive`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadNet {
    pub module: String,
    pub net: String,
    pub width: u32,
    pub span: Span,
}

pub fn analyse(design: &Design) -> LintReport {
    let mut latches = Vec::new();
    let mut dead_nets = Vec::new();

    for (module_id, module) in design.modules.iter_enumerated() {
        if module.is_blackbox {
            continue;
        }
        latches.extend(latches_in(module));
        dead_nets.extend(dead_nets_in(design, module, module_id));
    }

    let mut instantiated = BTreeSet::new();
    for module in design.modules.iter() {
        for instance in &module.insts {
            instantiated.insert(instance.of);
        }
    }
    let dead_modules = design
        .modules
        .iter_enumerated()
        .filter(|(id, module)| {
            *id != design.top && !module.is_blackbox && !instantiated.contains(id)
        })
        .map(|(_, module)| module.name.clone())
        .collect();

    LintReport { latches, dead_nets, dead_modules, comb_loops: comb_loops_in(design) }
}

// ------------------------------------------------- cycles made of wires ---

/// Every combinational loop in the design, with where its signals live.
///
/// The finding comes from the same graph the depth analysis walks, so a loop
/// reported here and a loop that refuses a depth count are the same loop.
fn comb_loops_in(design: &Design) -> Vec<CombLoop> {
    let flat = crate::flat::flatten(design);
    let graph = crate::depth::signal_graph(design, &flat);

    let mut out = Vec::new();
    for group in crate::depth::combinational_loops(&graph) {
        let mut signals = Vec::new();
        let mut spans = Vec::new();
        let mut partial = false;
        for signal in &group {
            signals.push(flat.name_of(*signal));
            let Some((module_id, net)) =
                crate::drive::nets_of(design, &flat, *signal).first().copied()
            else {
                continue;
            };
            let module = design.module(module_id);
            spans.push(module.net(net).span);
            partial |= written_through_a_slice(module, net);
        }
        out.push(CombLoop { signals, spans, partial });
    }
    out
}

/// Whether any assignment to this net writes only part of it.
fn written_through_a_slice(module: &Module, net: NetId) -> bool {
    fn walk(stmt: &Stmt, net: NetId, found: &mut bool) {
        if let StmtKind::Assign { lhs, .. } = &stmt.kind
            && matches!(lhs, rtlscope_ir::NetRef::Slice { net: at, .. } if *at == net)
        {
            *found = true;
        }
        stmt.for_each_child(&mut |child| walk(child, net, found));
    }

    let mut found = false;
    for process in &module.procs {
        walk(&process.body, net, &mut found);
    }
    found
}

// ------------------------------------------------------------- latches ---

fn latches_in(module: &Module) -> Vec<Latch> {
    let mut out = Vec::new();
    for process in &module.procs {
        // `always_latch` is a latch on purpose and says so; a flop holds its
        // value by definition.
        if !matches!(process.kind, ProcKind::Comb) {
            continue;
        }
        // A body with a hole in it cannot be reasoned about: the missing
        // statement may be the assignment that closes the gap.
        if has_hole(&process.body) {
            continue;
        }

        for written in &process.writes {
            let Some(net) = written.net_id() else { continue };
            // An array is written one element at a time by design; "every path
            // assigns every element" is not a question this can answer.
            if module.net(net).kind != NetKind::Logic {
                continue;
            }
            if let Coverage::Partial { because, span } = covers(&process.body, net) {
                out.push(Latch {
                    module: module.name.clone(),
                    net: module.net(net).name.clone(),
                    because,
                    span,
                    process_span: process.span,
                });
            }
        }
    }
    out
}

/// Whether every path through a statement assigns a net.
enum Coverage {
    /// Assigned no matter which way control goes.
    Total,
    /// Some path leaves it holding its old value, which is the latch.
    Partial { because: String, span: Span },
    /// This statement does not touch it at all, which only matters in context.
    Absent,
}

fn covers(stmt: &Stmt, net: NetId) -> Coverage {
    match &stmt.kind {
        StmtKind::Assign { lhs, .. } => {
            if lhs.net_id() == Some(net) {
                Coverage::Total
            } else {
                Coverage::Absent
            }
        }

        // Statements run in order, so one that assigns unconditionally covers
        // every conditional one after it. This is what makes the usual
        // `next = state;` before a `case` correct, and it is why the search
        // runs backwards: the last total assignment wins.
        StmtKind::Block { stmts } => {
            let mut partial: Option<Coverage> = None;
            for stmt in stmts.iter().rev() {
                match covers(stmt, net) {
                    Coverage::Total => return Coverage::Total,
                    // Keep the *first* gap found walking backwards, which is
                    // the last one in source order and the one to report.
                    Coverage::Partial { because, span } => {
                        partial.get_or_insert(Coverage::Partial { because, span });
                    }
                    Coverage::Absent => {}
                }
            }
            partial.unwrap_or(Coverage::Absent)
        }

        StmtKind::If { then_branch, else_branch, .. } => {
            let then_side = covers(then_branch, net);
            let else_side = else_branch.as_ref().map(|e| covers(e, net));

            match (&then_side, &else_side) {
                (Coverage::Absent, None) => Coverage::Absent,
                (Coverage::Absent, Some(Coverage::Absent)) => Coverage::Absent,
                (Coverage::Total, Some(Coverage::Total)) => Coverage::Total,
                // The classic one: an `if` that assigns and no `else`.
                (_, None) => Coverage::Partial {
                    because: "this `if` has no `else`".to_string(),
                    span: stmt.span,
                },
                _ => {
                    let (because, span) =
                        gap(&then_side, else_side.as_ref()).unwrap_or_else(|| {
                            ("one branch does not assign it".to_string(), stmt.span)
                        });
                    Coverage::Partial { because, span }
                }
            }
        }

        StmtKind::Case { arms, default, .. } => {
            let mut touched = false;
            let mut gap_found = None;
            for arm in arms {
                match covers(&arm.body, net) {
                    Coverage::Total => touched = true,
                    Coverage::Partial { because, span } => {
                        touched = true;
                        gap_found.get_or_insert((because, span));
                    }
                    Coverage::Absent => {
                        if gap_found.is_none() {
                            gap_found = Some((
                                "this `case` arm does not assign it".to_string(),
                                arm.body.span,
                            ));
                        }
                    }
                }
            }
            if !touched
                && !matches!(
                    default.as_deref().map(|d| covers(d, net)),
                    Some(Coverage::Total | Coverage::Partial { .. })
                )
            {
                return Coverage::Absent;
            }

            match default.as_deref().map(|d| covers(d, net)) {
                Some(Coverage::Total) => match gap_found {
                    None => Coverage::Total,
                    Some((because, span)) => Coverage::Partial { because, span },
                },
                Some(Coverage::Partial { because, span }) => Coverage::Partial { because, span },
                Some(Coverage::Absent) | None => Coverage::Partial {
                    because: match default {
                        // A `default` that assigns nothing is the same gap as
                        // no `default` at all, and worth naming differently so
                        // it is clear which one to go and look at.
                        Some(_) => "the `default` of this `case` does not assign it".to_string(),
                        None => "this `case` has no `default`".to_string(),
                    },
                    span: stmt.span,
                },
            }
        }

        StmtKind::Unsupported { .. } => Coverage::Absent,
    }
}

/// The reason from whichever side of an `if` fell short.
fn gap(then_side: &Coverage, else_side: Option<&Coverage>) -> Option<(String, Span)> {
    for side in [Some(then_side), else_side] {
        if let Some(Coverage::Partial { because, span }) = side {
            return Some((because.clone(), *span));
        }
    }
    None
}

/// Whether the front end left a hole anywhere in this body.
fn has_hole(body: &Stmt) -> bool {
    let mut found = false;
    body.for_each_stmt(&mut |stmt| {
        found |= matches!(stmt.kind, StmtKind::Unsupported { .. });
    });
    found
}

// ---------------------------------------------------------- dead nets ---

fn dead_nets_in(design: &Design, module: &Module, module_id: ModuleId) -> Vec<DeadNet> {
    let mut read = BTreeSet::new();
    let mut written = BTreeSet::new();

    for process in &module.procs {
        read.extend(process.reads.iter().filter_map(|r| r.net_id()));
        written.extend(process.writes.iter().filter_map(|w| w.net_id()));
    }

    // An instance reads what it takes in and writes what it gives back.
    for instance in &module.insts {
        let child = &design.modules[instance.of];
        for Conn { port, net: net_ref, .. } in &instance.conns {
            let Some(net) = net_ref.net_id() else { continue };
            let Some(port) = child.ports.get(port.0 as usize) else { continue };
            match port.dir {
                PortDir::Input => {
                    read.insert(net);
                }
                PortDir::Output => {
                    written.insert(net);
                }
                PortDir::Inout => {
                    read.insert(net);
                    written.insert(net);
                }
            }
        }
    }

    // The world outside reads this module's outputs and drives its inputs.
    for port in &module.ports {
        match port.dir {
            PortDir::Input => {
                written.insert(port.net);
            }
            PortDir::Output => {
                read.insert(port.net);
            }
            PortDir::Inout => {
                read.insert(port.net);
                written.insert(port.net);
            }
        }
    }

    let _ = module_id;
    module
        .nets
        .iter_enumerated()
        // A net RTLScope made is not something anyone can go and delete.
        .filter(|(_, net)| !net.synthesised)
        .filter(|(id, _)| written.contains(id) && !read.contains(id))
        .map(|(_, net)| DeadNet {
            module: module.name.clone(),
            net: net.name.clone(),
            width: net.width,
            span: net.span,
        })
        .collect()
}
