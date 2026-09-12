//! Where a signal comes from.
//!
//! "Why is this `x`?" and "why is it *that* value?" are the two questions a
//! waveform raises and cannot answer, and both are answered the same way: find
//! what drives the signal, then find what decides what that driver writes, then
//! do it again one step back. This module is those two steps.
//!
//! Both were already here, written twice for two purposes and both private:
//! the block diagram worked out every net's drivers and loads in order to draw
//! edges, and the pipeline analysis walked each assignment for what decides it
//! in order to measure depth. Neither could be asked a question. They live here
//! now and their old owners read from here, so there is one answer to "who
//! drives this net" rather than two that can disagree.
//!
//! # What this does not know
//!
//! Three limits, all reported rather than papered over, because a provenance
//! that quietly leaves out a driver is worse than none:
//!
//! - **Bit slices collapse to whole nets.** `q[7:0] <= a` and `q[15:8] <= b`
//!   both read as driving `q`, so a net driven bit by bit reports every one of
//!   them as a full driver.
//! - **A constant tied to a port is not a driver.** Elaboration records the
//!   constant, not a net, so the child's port looks undriven.
//! - **More than one driver is reported, not judged.** Two drivers on one net
//!   may be a bus, a mistake, or the slice case above; saying which is not
//!   something this can do from the IR.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rtlscope_ir::{
    Conn, Design, Expr, InstId, Module, ModuleId, NetId, PortDir, PortId, ProcId, ProcKind, Span,
    Stmt, StmtKind,
};

use crate::flat::{Flattened, SignalId};

/// One end of a net inside a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum End {
    /// A port of the module itself.
    Port(PortId),
    /// A port of a child instance, named from the child's side.
    Inst(InstId, PortId),
    /// An `always` block or a continuous assign.
    Proc(ProcId),
}

/// One end, and what the net is called there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub at: End,
    /// The port's name at that end, or the net's own name for a process.
    pub pin: String,
}

/// Who drives a net, and who reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Endpoints {
    pub drivers: Vec<Endpoint>,
    pub loads: Vec<Endpoint>,
}

/// Every net in a module, with who drives it and who reads it.
///
/// The four ways a net can be driven are all here: an input port of the module,
/// an output port of a child, a process, and — for an `inout` — both at once.
/// A child's direction is reversed on the way out, since the child's *output*
/// drives the parent's net.
pub fn endpoints(design: &Design, module: &Module) -> HashMap<NetId, Endpoints> {
    let mut out: HashMap<NetId, Endpoints> = HashMap::new();

    for (position, port) in module.ports.iter().enumerate() {
        let id = PortId(position as u32);
        let end = Endpoint { at: End::Port(id), pin: port.name.clone() };
        let entry = out.entry(port.net).or_default();
        match port.dir {
            PortDir::Input => entry.drivers.push(end),
            PortDir::Output => entry.loads.push(end),
            PortDir::Inout => {
                entry.drivers.push(end.clone());
                entry.loads.push(end);
            }
        }
    }

    for (position, inst) in module.insts.iter().enumerate() {
        let id = InstId(position as u32);
        let child = design.module(inst.of);
        for Conn { port, net: net_ref, .. } in &inst.conns {
            let Some(net) = net_ref.net_id() else { continue };
            let Some(child_port) = child.ports.get(port.0 as usize) else { continue };
            let end = Endpoint { at: End::Inst(id, *port), pin: child_port.name.clone() };
            let entry = out.entry(net).or_default();
            match child_port.dir {
                PortDir::Input => entry.loads.push(end),
                PortDir::Output => entry.drivers.push(end),
                PortDir::Inout => {
                    entry.drivers.push(end.clone());
                    entry.loads.push(end);
                }
            }
        }
    }

    for (position, process) in module.procs.iter().enumerate() {
        let id = ProcId(position as u32);
        for (refs, driving) in [(&process.writes, true), (&process.reads, false)] {
            for net_ref in refs {
                let Some(net) = net_ref.net_id() else { continue };
                let end = Endpoint { at: End::Proc(id), pin: module.net(net).name.clone() };
                let entry = out.entry(net).or_default();
                match driving {
                    true => entry.drivers.push(end),
                    false => entry.loads.push(end),
                }
            }
        }
    }

    out
}

/// A condition on the way to an assignment, as the source wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    /// The whole thing — `if (en)`, `case (state)` — rather than the bare
    /// expression. `en` on its own leaves the reader to work out what was done
    /// with it, and being under a `case` subject is a different claim from
    /// being under an `if`.
    pub text: String,
    pub nets: BTreeSet<NetId>,
}

/// What decides one net's next value inside one process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decided {
    /// Nets on the right-hand side, and the index of a memory write.
    pub data: BTreeSet<NetId>,
    /// Every `if` and `case` between the top of the process and the
    /// assignment, in the order they were entered.
    pub conditions: Vec<Condition>,
    /// True only when *every* assignment to it was blocking. A net written
    /// both ways in one process is a register: the non-blocking one decides.
    pub blocking: bool,
}

impl Decided {
    /// Everything the value depends on, however it got there.
    ///
    /// A condition decides the value as surely as the right-hand side does:
    /// `if (en) q <= d` depends on `en` as much as on `d`.
    pub fn sources(&self) -> BTreeSet<NetId> {
        let mut all = self.data.clone();
        for condition in &self.conditions {
            all.extend(condition.nets.iter().copied());
        }
        all
    }
}

/// Every statement in a module that assigns to a net, in source order.
///
/// Not the same question as [`trace`], which answers "what puts a value here"
/// and gives one driver per process — the right grain for provenance, since a
/// whole `always_ff` is one thing that decides a value. This is the reader's
/// other question, "where is this written", and there the grain is the
/// statement: a register assigned in a reset branch and again in a data branch
/// is written in two places, and being shown only the `always_ff` header means
/// finding them by eye.
///
/// Continuous assigns come back too. Elaboration lowers them to processes, so
/// they are already here and need no separate case.
pub fn assignments(module: &Module, net: NetId) -> Vec<Span> {
    fn walk(stmt: &rtlscope_ir::Stmt, net: NetId, found: &mut Vec<Span>) {
        use rtlscope_ir::StmtKind;
        match &stmt.kind {
            StmtKind::Assign { lhs, .. } => {
                // A slice assignment writes the net as surely as a whole one:
                // `staged[3:0] <= x` is a place `staged` is written.
                if lhs.net_id() == Some(net) {
                    found.push(stmt.span);
                }
            }
            StmtKind::Block { stmts } => {
                for stmt in stmts {
                    walk(stmt, net, found);
                }
            }
            StmtKind::If { then_branch, else_branch, .. } => {
                walk(then_branch, net, found);
                if let Some(otherwise) = else_branch {
                    walk(otherwise, net, found);
                }
            }
            StmtKind::Case { arms, default, .. } => {
                for arm in arms {
                    walk(&arm.body, net, found);
                }
                if let Some(default) = default {
                    walk(default, net, found);
                }
            }
            _ => {}
        }
    }

    let mut found = Vec::new();
    for process in &module.procs {
        walk(&process.body, net, &mut found);
    }
    found.sort_by_key(|span| (span.file, span.line, span.col));
    found.dedup();
    found
}

/// What decides each net a process assigns.
///
/// Per *assignment*, not per process: one `always_ff` usually holds a whole
/// pipeline, and taking the process's own `reads` would make every register in
/// it depend on every other and collapse the pipeline to one stage.
pub fn decides(module: &Module, process: &rtlscope_ir::Process) -> BTreeMap<NetId, Decided> {
    let mut out = BTreeMap::new();
    walk(module, &process.body, &mut Vec::new(), &mut out);
    out
}

/// How a condition was written, which decides how it reads back.
#[derive(Debug, Clone, Copy)]
enum Guard {
    If,
    Case,
}

fn walk<'a>(
    module: &Module,
    stmt: &'a Stmt,
    guard: &mut Vec<(Guard, &'a Expr)>,
    out: &mut BTreeMap<NetId, Decided>,
) {
    match &stmt.kind {
        StmtKind::Assign { lhs, lhs_index, rhs, blocking } => {
            let Some(target) = lhs.net_id() else { return };
            let first = !out.contains_key(&target);
            let decided = out.entry(target).or_default();
            decided.blocking = if first { *blocking } else { decided.blocking && *blocking };

            let gather = |expr: &Expr, into: &mut BTreeSet<NetId>| {
                expr.for_each_ref(&mut |net_ref| {
                    if let Some(net) = net_ref.net_id() {
                        into.insert(net);
                    }
                });
            };
            gather(rhs, &mut decided.data);
            // `mem[addr] <= d` depends on the address as much as on the data.
            if let Some(index) = lhs_index {
                gather(index, &mut decided.data);
            }
            for (kind, expr) in guard.iter().copied() {
                let mut nets = BTreeSet::new();
                gather(expr, &mut nets);
                let text = match kind {
                    Guard::If => format!("if ({})", module.render(expr)),
                    Guard::Case => format!("case ({})", module.render(expr)),
                };
                let condition = Condition { text, nets };
                if !decided.conditions.contains(&condition) {
                    decided.conditions.push(condition);
                }
            }
        }
        StmtKind::Block { stmts } => {
            for stmt in stmts {
                walk(module, stmt, guard, out);
            }
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            guard.push((Guard::If, cond));
            walk(module, then_branch, guard, out);
            if let Some(else_branch) = else_branch {
                walk(module, else_branch, guard, out);
            }
            guard.pop();
        }
        StmtKind::Case { subject, arms, default, .. } => {
            guard.push((Guard::Case, subject));
            for arm in arms {
                walk(module, &arm.body, guard, out);
            }
            if let Some(default) = default {
                walk(module, default, guard, out);
            }
            guard.pop();
        }
        StmtKind::Unsupported { .. } => {}
    }
}

/// What a net's value came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Trace {
    /// The name a reader would recognise it by.
    pub name: String,
    pub width: u32,
    pub drivers: Vec<Driver>,
    /// What this could not account for. Empty means the answer is whole.
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Driver {
    pub kind: Kind,
    pub span: Span,
    /// The instance path it sits at, `u_rx.u_align`; empty at the top.
    pub path: String,
    /// What decides the value, when the driver is something that decides.
    pub depends: Vec<Depends>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A clocked process — the value arrives on an edge.
    Register { clock: String, reset: Option<String> },
    /// Combinational logic, or a continuous assign.
    Combinational,
    /// Logic that does not assign on every path, so synthesis builds storage.
    Latch,
    /// An `initial` block: simulation only.
    Initial,
    /// A port of the module being looked at, driven from outside it.
    FromOutside { port: String },
    /// The output of a child instance.
    FromInstance { instance: String, of: String, port: String },
    /// Nothing in the design drives it.
    Nothing,
}

/// One thing a driver's value depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Depends {
    pub name: String,
    /// The net, so a caller can follow it without matching on the name.
    pub net: NetId,
    /// The condition it appears in, as the source wrote it. `None` when it is
    /// on the right-hand side.
    pub through: Option<String>,
}

/// Follows a net back to whatever puts a value on it.
///
/// One step, not the whole chain: a reader following provenance wants to see
/// each hop and decide which one to take, and a report that unrolled the lot
/// would be a report of the whole design.
pub fn trace(design: &Design, flat: &Flattened, module_id: ModuleId, net: NetId) -> Trace {
    let module = design.module(module_id);
    let mut problems = Vec::new();
    let ends = endpoints(design, module);
    let found = ends.get(&net).cloned().unwrap_or_default();

    // Where this module sits, so a report names a place rather than a module
    // that may be instantiated five times.
    let path = flat
        .nodes
        .iter()
        .find(|node| node.module == module_id)
        .map(|node| node.path.clone())
        .unwrap_or_default();

    let mut drivers = Vec::new();
    for driver in &found.drivers {
        match driver.at {
            End::Port(port) => {
                let at = &module.ports[port.0 as usize];
                drivers.push(Driver {
                    kind: Kind::FromOutside { port: at.name.clone() },
                    span: at.span,
                    path: path.clone(),
                    depends: Vec::new(),
                });
            }
            End::Inst(inst, port) => {
                let at = &module.insts[inst.0 as usize];
                let child = design.module(at.of);
                drivers.push(Driver {
                    kind: Kind::FromInstance {
                        instance: at.name.clone(),
                        of: child.base_name.clone(),
                        port: driver.pin.clone(),
                    },
                    span: at.span,
                    path: path.clone(),
                    depends: Vec::new(),
                });
                let _ = port;
            }
            End::Proc(proc) => {
                let process = &module.procs[proc.0 as usize];
                let kind = match &process.kind {
                    ProcKind::Ff { clk, rst, .. } => Kind::Register {
                        clock: module.render_ref(clk),
                        reset: rst.as_ref().map(|reset| module.render_ref(&reset.net)),
                    },
                    ProcKind::Comb => Kind::Combinational,
                    ProcKind::Latch => Kind::Latch,
                    ProcKind::Initial => Kind::Initial,
                };
                let decided = decides(module, process);
                let depends = match decided.get(&net) {
                    Some(decided) => flatten_depends(module, decided),
                    None => {
                        // `writes` says it drives this net but no assignment
                        // names it: a slice, or something not modelled.
                        problems.push(format!(
                            "`{}` is written by the process at {} in a way this cannot read \
                             apart — a bit slice, most likely",
                            module.net(net).name,
                            design.files.render(process.span)
                        ));
                        Vec::new()
                    }
                };
                drivers.push(Driver { kind, span: process.span, path: path.clone(), depends });
            }
        }
    }

    if drivers.is_empty() {
        drivers.push(Driver {
            kind: Kind::Nothing,
            span: module.net(net).span,
            path: path.clone(),
            depends: Vec::new(),
        });
        problems.push(
            "Nothing in the design drives this. A port tied to a constant looks like this too: \
             elaboration records the constant rather than a net."
                .to_string(),
        );
    }
    if drivers.len() > 1 {
        problems.push(format!(
            "{} drivers. That is a bus, a mistake, or one net written a slice at a time — \
             which of the three is not something the IR says.",
            drivers.len()
        ));
    }

    Trace { name: module.net(net).name.clone(), width: module.net(net).width, drivers, problems }
}

/// The dependencies of one assignment, named and in a readable order: what the
/// value is made of first, then what had to be true for it to happen.
fn flatten_depends(module: &Module, decided: &Decided) -> Vec<Depends> {
    let mut out: Vec<Depends> = decided
        .data
        .iter()
        .map(|net| Depends { name: module.net(*net).name.clone(), net: *net, through: None })
        .collect();
    for condition in &decided.conditions {
        for net in &condition.nets {
            // A net that is both the data and a condition is worth saying
            // twice: it means something different each time.
            out.push(Depends {
                name: module.net(*net).name.clone(),
                net: *net,
                through: Some(condition.text.clone()),
            });
        }
    }
    out
}

/// Every net a signal goes by, so a trace can start from a waveform.
///
/// A `SignalId` is the whole wire; a net is one module's name for part of it.
/// Which module to trace in is the caller's choice, and the shallowest is the
/// one a reader recognises.
pub fn nets_of(design: &Design, flat: &Flattened, signal: SignalId) -> Vec<(ModuleId, NetId)> {
    let mut out = Vec::new();
    for node in &flat.nodes {
        let module = design.module(node.module);
        for net in module.nets.indices() {
            if node.signal(net) == Some(signal) {
                out.push((node.module, net));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtlscope_sv::ParseOptions;

    fn design(fixture: &str, top: Option<&str>) -> Design {
        let path = rtlscope_fixtures::path(fixture);
        let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
        rtlscope_elab::elaborate(&uir, top).0.expect("elaborates")
    }

    /// Where a net is written, at the grain a reader means by "where".
    ///
    /// `staged` is assigned twice in one `always_ff`: once clearing on reset
    /// and once taking `in_data`. [`trace`] calls that one driver, which is
    /// right for provenance and wrong for this question — somebody asking
    /// where a register is set wants both lines.
    #[test]
    fn a_net_written_in_two_branches_is_written_in_two_places() {
        let design = design("trace_demo.sv", Some("trace_demo"));
        let module = design.module(design.top);
        let net = module
            .nets
            .indices()
            .find(|id| module.net(*id).name == "staged")
            .expect("`staged` is a net of the top");

        let places = assignments(module, net);
        assert_eq!(places.len(), 2, "both branches: {places:?}");
        assert!(places[0].line < places[1].line, "in source order: {places:?}");

        // And the process they share reports as one driver, which is the
        // difference this function exists for.
        let flat = crate::flat::flatten(&design);
        let drivers = trace(&design, &flat, design.top, net).drivers;
        assert_eq!(drivers.len(), 1, "one process: {drivers:?}");
    }

    /// A net nothing writes has nowhere to send a reader, and saying so with an
    /// empty list beats inventing a line.
    #[test]
    fn a_net_nothing_writes_has_no_places() {
        let design = design("trace_demo.sv", Some("trace_demo"));
        let module = design.module(design.top);
        let net = module
            .nets
            .indices()
            .find(|id| module.net(*id).name == "in_data")
            .expect("`in_data` is a port of the top");

        assert!(assignments(module, net).is_empty(), "an input is written from outside");
    }

    /// The whole road from an output back to the edge of the design.
    ///
    /// `trace_demo.sv` exists for this: the path from `out_data` to `in_valid`
    /// passes through every kind of answer the view can give, in order, so one
    /// walk covers the lot. A waveform of it shows `out_data` freezing partway
    /// through the run with nothing in `trace_demo` to explain it — the reason
    /// is two hops away, inside the child, which is the case provenance exists
    /// for and the one a sample has to actually contain.
    #[test]
    fn the_sample_walks_through_every_kind_of_driver() {
        let design = design("trace_demo.sv", Some("trace_demo"));
        let flat = crate::flat::flatten(&design);
        let top = design.top;
        let (child, _) = design.module_by_name("trace_gate").expect("the child module");

        let one = |module: ModuleId, name: &str| {
            let traced = trace(&design, &flat, module, net(&design, module, name));
            assert!(traced.problems.is_empty(), "{name}: {:?}", traced.problems);
            assert_eq!(traced.drivers.len(), 1, "{name} has one driver");
            traced.drivers.into_iter().next().expect("just checked")
        };
        let through = |driver: &Driver, name: &str| {
            driver
                .depends
                .iter()
                .filter(|on| on.name == name)
                .map(|on| on.through.clone())
                .collect::<Vec<_>>()
        };

        // 1. An assign.
        let out = one(top, "out_data");
        assert_eq!(out.kind, Kind::Combinational);
        assert_eq!(through(&out, "staged"), [None], "the right-hand side, not a condition");

        // 2. A clocked process, and the split between data and condition.
        let staged = one(top, "staged");
        assert_eq!(
            staged.kind,
            Kind::Register { clock: "clk".into(), reset: Some("rst_n".into()) }
        );
        assert_eq!(through(&staged, "in_data"), [None], "what is written");
        assert_eq!(
            through(&staged, "gate"),
            [Some("if (gate)".to_string())],
            "and what decides whether it is"
        );

        // 3. Out of a child instance — the hop the diagram calls `go inside`.
        let gate = one(top, "gate");
        assert_eq!(
            gate.kind,
            Kind::FromInstance {
                instance: "u_gate".into(),
                of: "trace_gate".into(),
                port: "allow".into(),
            }
        );

        // 4. Inside it, a `case` as well as an `if`.
        let allow = one(child, "allow");
        assert!(matches!(allow.kind, Kind::Register { .. }), "{:?}", allow.kind);
        assert_eq!(through(&allow, "mode"), [Some("case (mode)".to_string())]);
        assert_eq!(through(&allow, "rst_n"), [Some("if (!rst_n)".to_string())]);
        assert_eq!(through(&allow, "want"), [None], "the value, in the arm it is written in");

        // 5. Back out through a port — the hop called `go up`.
        let want = one(child, "want");
        assert_eq!(want.kind, Kind::FromOutside { port: "want".into() });

        // 6. And the edge of the design, where a walk has to stop.
        let at_the_edge = one(top, "in_valid");
        assert_eq!(at_the_edge.kind, Kind::FromOutside { port: "in_valid".into() });
    }

    fn net(design: &Design, module: ModuleId, name: &str) -> NetId {
        design
            .module(module)
            .nets
            .iter_enumerated()
            .find(|(_, net)| net.name == name)
            .unwrap_or_else(|| panic!("no net `{name}`"))
            .0
    }

    /// A register: the clock is named, and what decides the value is the
    /// right-hand side — not everything the process happens to read.
    #[test]
    fn a_registers_provenance_names_its_clock_and_its_own_sources() {
        let design = design("pipeline3.sv", Some("pipeline3"));
        let flat = crate::flat::flatten(&design);
        let top = design.top;

        let traced = trace(&design, &flat, top, net(&design, top, "data_d2"));

        assert_eq!(traced.drivers.len(), 1, "{:?}", traced.drivers);
        let driver = &traced.drivers[0];
        assert!(
            matches!(&driver.kind, Kind::Register { clock, .. } if clock == "clk"),
            "{:?}",
            driver.kind
        );

        // `data_d2 <= data_d1 + 1`. The whole `always_ff` reads every stage,
        // but only this one decides this value.
        let names: Vec<&str> = driver.depends.iter().map(|on| on.name.as_str()).collect();
        assert!(names.contains(&"data_d1"), "{names:?}");
        assert!(!names.contains(&"data_d2"), "a stage does not feed itself here: {names:?}");
        assert!(traced.problems.is_empty(), "{:?}", traced.problems);
    }

    /// A condition decides a value as surely as the right-hand side does, and
    /// the report says which condition rather than only naming the signal.
    #[test]
    fn a_condition_is_a_dependency_and_is_quoted() {
        let design = design("fsm.sv", None);
        let flat = crate::flat::flatten(&design);
        let top = design.top;

        let traced = trace(&design, &flat, top, net(&design, top, "next_state"));
        let driver = traced.drivers.iter().find(|d| !d.depends.is_empty()).expect("a driver");

        let through: Vec<&str> =
            driver.depends.iter().filter_map(|on| on.through.as_deref()).collect();
        assert!(!through.is_empty(), "no condition was recorded: {:?}", driver.depends);
        assert!(
            through.iter().any(|text| text.contains("state")),
            "the `case (state)` it is under: {through:?}"
        );
    }

    /// Coming from outside is provenance too, and it is where a trace inside
    /// one module has to stop.
    #[test]
    fn a_port_says_the_value_comes_from_outside() {
        let design = design("pipeline3.sv", Some("pipeline3"));
        let flat = crate::flat::flatten(&design);
        let top = design.top;

        let traced = trace(&design, &flat, top, net(&design, top, "in_data"));
        assert!(
            matches!(&traced.drivers[0].kind, Kind::FromOutside { port } if port == "in_data"),
            "{:?}",
            traced.drivers[0].kind
        );
    }

    /// A child's output drives the parent's net, and the trace says which
    /// child and which port so the next hop is obvious.
    #[test]
    fn an_instances_output_is_named_with_the_instance_it_came_from() {
        let design = design("hier.sv", Some("hier_top"));
        let flat = crate::flat::flatten(&design);
        let top = design.top;

        let from_child: Vec<Driver> = design
            .module(top)
            .nets
            .indices()
            .flat_map(|net| trace(&design, &flat, top, net).drivers)
            .filter(|driver| matches!(driver.kind, Kind::FromInstance { .. }))
            .collect();

        assert!(!from_child.is_empty(), "hier.sv has a child driving something");
        let Kind::FromInstance { instance, of, port } = &from_child[0].kind else { unreachable!() };
        assert!(!instance.is_empty() && !of.is_empty() && !port.is_empty());
    }

    /// The block diagram and this must agree about who drives what: they are
    /// the same question, and were two implementations of it until now.
    #[test]
    fn every_driver_the_diagram_draws_is_one_this_reports() {
        let design = design("hier.sv", Some("hier_top"));
        let module = design.module(design.top);
        let ends = endpoints(&design, module);

        for net in module.nets.indices() {
            let found = ends.get(&net).cloned().unwrap_or_default();
            // Every driver names an end that exists.
            for driver in &found.drivers {
                match driver.at {
                    End::Port(port) => assert!(module.ports.get(port.0 as usize).is_some()),
                    End::Inst(inst, _) => assert!(module.insts.get(inst.0 as usize).is_some()),
                    End::Proc(proc) => assert!(module.procs.get(proc.0 as usize).is_some()),
                }
            }
        }
    }
}
