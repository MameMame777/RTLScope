//! Finding state machines in the elaborated IR.
//!
//! A state machine has a recognisable shape once the parameters are folded and
//! the generate blocks are gone: a register whose value is tested by a `case`,
//! and whose next value is decided inside that `case`'s arms as constants. Both
//! of the usual writing styles produce it —
//!
//! ```systemverilog
//! always_ff @(posedge clk)          // one process
//!     case (state)
//!         IDLE: if (go) state <= RUN;
//!
//! always_ff @(posedge clk) state <= next;   // two processes
//! always_comb
//!     case (state)
//!         IDLE: if (go) next = RUN;
//! ```
//!
//! — and the difference between them is one indirection, so both are read here.
//!
//! What this does *not* do is decide whether something is "really" a state
//! machine. A counter written as a `case` is reported as one, because from the
//! logic it is one; the judgement belongs to whoever reads the report.

use std::collections::{BTreeSet, HashMap};

use rtlscope_ir::{
    ConstBits, Design, Expr, ExprKind, Module, ModuleId, NetId, NetRef, ProcKind, Span, Stmt,
    StmtKind,
};
use serde::{Deserialize, Serialize};

/// Where a `case`'s `default:` arm leads from.
///
/// Not a state — a stand-in for every state the arms do not name, which may be
/// none of them.
pub const ANY_OTHER: &str = "(any other)";

/// One state machine, as the logic describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fsm {
    pub module: ModuleId,
    pub module_name: String,
    /// The register holding the state.
    pub state: NetId,
    pub state_name: String,
    /// The combinational net feeding the register, in the two-process style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_name: Option<String>,
    pub clock: String,
    /// The state the reset branch puts it in, when the process has a reset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_state: Option<String>,
    pub states: Vec<FsmState>,
    pub transitions: Vec<Transition>,
    /// States that no transition leads to, other than through reset.
    ///
    /// Either dead code or a state reached some way this analysis cannot see;
    /// worth naming either way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreachable: Vec<String>,
    /// States with no way out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminal: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsmState {
    pub name: String,
    pub value: i64,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub from: String,
    pub to: String,
    /// The conditions between the `case` arm and the assignment, outermost
    /// first. Empty means the arm takes this transition unconditionally.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guard: Vec<String>,
    pub span: Span,
}

/// Every state machine in the design, in module order.
pub fn find(design: &Design) -> Vec<Fsm> {
    let mut out = Vec::new();
    for (module_id, module) in design.modules.iter_enumerated() {
        out.extend(find_in_module(module, module_id));
    }
    out
}

/// Every state machine in one module.
pub fn find_in_module(module: &Module, module_id: ModuleId) -> Vec<Fsm> {
    let flops = flops_of(module);
    let mut found: Vec<Fsm> = Vec::new();

    for process in &module.procs {
        let mut cases = Vec::new();
        collect_cases(&process.body, &mut cases);

        for case in cases {
            let Some(subject) = full_ref(case.subject) else { continue };
            let Some(flop) = flops.get(&subject) else { continue };

            // What the arms assign constants to. In the one-process style that
            // is the state register itself; in the two-process style it is the
            // combinational net a flop copies from.
            let mut targets = BTreeSet::new();
            for arm in &case.arms {
                constant_targets(arm, &mut targets);
            }

            let (target, next_name) = if targets.contains(&subject) {
                (subject, None)
            } else {
                let Some(next) = targets.iter().copied().find(|t| copies_from(module, subject, *t))
                else {
                    continue;
                };
                (next, Some(module.net(next).name.clone()))
            };

            // One `case` per state register: a second one testing the same
            // register is more of the same machine, not another machine.
            if found.iter().any(|f| f.state == subject) {
                continue;
            }

            found.push(build(module, module_id, subject, target, next_name, flop, &case));
        }
    }
    found
}

/// What a flop process tells us about the register it writes.
struct Flop<'a> {
    clock: &'a NetRef,
    /// The whole process body, for finding the reset branch.
    body: &'a Stmt,
    has_reset: bool,
    span: Span,
}

fn flops_of(module: &Module) -> HashMap<NetId, Flop<'_>> {
    let mut out = HashMap::new();
    for process in &module.procs {
        let ProcKind::Ff { clk, rst, .. } = &process.kind else { continue };
        for written in &process.writes {
            if let Some(net) = written.net_id() {
                out.insert(
                    net,
                    Flop {
                        clock: clk,
                        body: &process.body,
                        has_reset: rst.is_some(),
                        span: process.span,
                    },
                );
            }
        }
    }
    out
}

/// A `case` statement, lifted out of the tree it was found in.
struct Case<'a> {
    subject: &'a Expr,
    arms: Vec<&'a Stmt>,
    /// Each arm's label values, alongside the arm body index.
    labels: Vec<Vec<i64>>,
    spans: Vec<Span>,
    /// `default:`, which stands for every state no arm names.
    default: Option<&'a Stmt>,
}

fn collect_cases<'a>(stmt: &'a Stmt, out: &mut Vec<Case<'a>>) {
    if let StmtKind::Case { subject, arms, default, .. } = &stmt.kind {
        let mut bodies = Vec::new();
        let mut labels = Vec::new();
        let mut spans = Vec::new();
        for arm in arms {
            let values: Vec<i64> = arm.labels.iter().filter_map(constant_of).collect();
            // An arm labelled with something that did not fold is an arm this
            // analysis cannot place, so the machine is left alone rather than
            // reported with a hole in it.
            if values.len() != arm.labels.len() {
                return;
            }
            bodies.push(&arm.body);
            labels.push(values);
            spans.push(arm.body.span);
        }
        out.push(Case { subject, arms: bodies, labels, spans, default: default.as_deref() });
    }
    stmt.for_each_child(&mut |child| collect_cases(child, out));
}

fn build(
    module: &Module,
    module_id: ModuleId,
    state: NetId,
    target: NetId,
    next_name: Option<String>,
    flop: &Flop<'_>,
    case: &Case<'_>,
) -> Fsm {
    let type_name = module.net(state).type_name.as_deref();
    let name_of = |value: i64| match module.enum_shown(type_name, value) {
        Some(name) => name.to_string(),
        None => match param_named(module, value) {
            Some(name) => name.to_string(),
            None => format!("{}'d{value}", module.net(state).width),
        },
    };

    let mut states: Vec<FsmState> = Vec::new();
    let mut transitions = Vec::new();
    let remember = |states: &mut Vec<FsmState>, value: i64, span: Span| {
        if !states.iter().any(|s| s.value == value) {
            states.push(FsmState { name: name_of(value), value, span });
        }
    };

    for (index, arm) in case.arms.iter().enumerate() {
        for value in &case.labels[index] {
            remember(&mut states, *value, case.spans[index]);
        }
        let mut reached = Vec::new();
        assignments_to(arm, target, &mut Vec::new(), &mut reached);
        for (to, guard, span) in reached {
            remember(&mut states, to, span);
            for from in &case.labels[index] {
                transitions.push(Transition {
                    from: name_of(*from),
                    to: name_of(to),
                    guard: guard.iter().map(|e| module.render(e)).collect(),
                    span,
                });
            }
        }
    }

    // `default:` stands for every state no arm names, which may be none of them
    // once the arms cover the encoding. Recording it under a name of its own
    // says what the RTL says without inventing states to hang it on.
    if let Some(default) = case.default {
        let mut reached = Vec::new();
        assignments_to(default, target, &mut Vec::new(), &mut reached);
        for (to, guard, span) in reached {
            remember(&mut states, to, span);
            transitions.push(Transition {
                from: ANY_OTHER.to_string(),
                to: name_of(to),
                guard: guard.iter().map(|e| module.render(e)).collect(),
                span,
            });
        }
    }

    let reset_state = flop.has_reset.then(|| reset_value(flop.body, state)).flatten().map(&name_of);

    // `default` reaches every unnamed state, so with one present nothing can be
    // called unreachable on the strength of the arms alone.
    let has_default = case.default.is_some();
    let reachable: BTreeSet<&str> = transitions.iter().map(|t| t.to.as_str()).collect();
    let leaves: BTreeSet<&str> = transitions.iter().map(|t| t.from.as_str()).collect();
    let unreachable = states
        .iter()
        .filter(|_| !has_default)
        .filter(|s| !reachable.contains(s.name.as_str()) && Some(&s.name) != reset_state.as_ref())
        .map(|s| s.name.clone())
        .collect();
    let terminal = states
        .iter()
        .filter(|s| !leaves.contains(s.name.as_str()))
        .map(|s| s.name.clone())
        .collect();

    Fsm {
        module: module_id,
        module_name: module.shown().into_owned(),
        state,
        state_name: module.net(state).shown().to_string(),
        next_name,
        clock: module.render_ref(flop.clock),
        reset_state,
        states,
        transitions,
        unreachable,
        terminal,
        span: flop.span,
    }
}

/// Every constant assigned to `target`, with the conditions on the way to it.
fn assignments_to<'a>(
    stmt: &'a Stmt,
    target: NetId,
    guard: &mut Vec<&'a Expr>,
    out: &mut Vec<(i64, Vec<&'a Expr>, Span)>,
) {
    match &stmt.kind {
        StmtKind::Assign { lhs, rhs, .. } => {
            if lhs.net_id() == Some(target)
                && let Some(value) = constant_of(rhs)
            {
                out.push((value, guard.clone(), stmt.span));
            }
        }
        StmtKind::Block { stmts } => {
            for stmt in stmts {
                assignments_to(stmt, target, guard, out);
            }
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            guard.push(cond);
            assignments_to(then_branch, target, guard, out);
            guard.pop();
            // The `else` path is recorded without its condition rather than
            // with a negation this crate would have to invent an expression
            // for; the arm it belongs to is still right.
            if let Some(else_branch) = else_branch {
                assignments_to(else_branch, target, guard, out);
            }
        }
        StmtKind::Case { arms, default, .. } => {
            for arm in arms {
                assignments_to(&arm.body, target, guard, out);
            }
            if let Some(default) = default {
                assignments_to(default, target, guard, out);
            }
        }
        StmtKind::Unsupported { .. } => {}
    }
}

/// The nets an arm assigns constants to.
fn constant_targets(stmt: &Stmt, out: &mut BTreeSet<NetId>) {
    if let StmtKind::Assign { lhs, rhs, .. } = &stmt.kind
        && constant_of(rhs).is_some()
        && let Some(net) = lhs.net_id()
    {
        out.insert(net);
    }
    stmt.for_each_child(&mut |child| constant_targets(child, out));
}

/// Whether some flop in this module assigns `state` from exactly `next`.
fn copies_from(module: &Module, state: NetId, next: NetId) -> bool {
    module.procs.iter().any(|process| {
        matches!(process.kind, ProcKind::Ff { .. }) && copies(&process.body, state, next)
    })
}

fn copies(stmt: &Stmt, state: NetId, next: NetId) -> bool {
    if let StmtKind::Assign { lhs, rhs, .. } = &stmt.kind
        && lhs.net_id() == Some(state)
        && let ExprKind::Ref { net } = &rhs.kind
        && net.net_id() == Some(next)
    {
        return true;
    }
    let mut found = false;
    stmt.for_each_child(&mut |child| found |= copies(child, state, next));
    found
}

/// The constant a reset branch puts the register into.
///
/// The reset is the leading `if` of the process, which is how the front end
/// classified it in the first place, so the value is whatever that branch
/// assigns.
fn reset_value(body: &Stmt, state: NetId) -> Option<i64> {
    let mut stmt = body;
    // Step through any wrapper blocks to the first real statement.
    while let StmtKind::Block { stmts } = &stmt.kind {
        stmt = stmts.first()?;
    }
    let StmtKind::If { then_branch, .. } = &stmt.kind else { return None };
    let mut found = Vec::new();
    assignments_to(then_branch, state, &mut Vec::new(), &mut found);
    found.first().map(|(value, _, _)| *value)
}

fn constant_of(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Lit { value } => signed(value),
        ExprKind::Ref { net: NetRef::Const { value } } => signed(value),
        _ => None,
    }
}

fn signed(value: &ConstBits) -> Option<i64> {
    value.to_u64().and_then(|v| i64::try_from(v).ok())
}

fn full_ref(expr: &Expr) -> Option<NetId> {
    match &expr.kind {
        ExprKind::Ref { net: NetRef::Full { net } } => Some(*net),
        _ => None,
    }
}

/// A localparam in this module with exactly this value.
///
/// The other way state names survive elaboration: `localparam ST_IDLE = 0;` is
/// as common as an enum, and folds to a number just as thoroughly.
fn param_named(module: &Module, value: i64) -> Option<&str> {
    let mut found = None;
    for param in module.params.iter().filter(|p| p.is_local && p.value == value) {
        if found.is_some() {
            return None;
        }
        found = Some(param.name.as_str());
    }
    found
}
