//! The signal graph, and how many clocks lie between two points on it.
//!
//! [`crate::pipeline`] asks "how deep is this design" and answers it for the
//! whole register graph at once. This asks the other question a reader has in
//! front of a waveform — "how many clocks from *here* to *there*" — which is a
//! different shape: it starts and ends at named signals, either of which may be
//! a port or a wire rather than a register, and it has to say which way the
//! value went, not just how far.
//!
//! Both need the same graph, and until now `pipeline::analyse` built it inside
//! itself and dropped it on the way out. [`signal_graph`] is that construction,
//! lifted out unchanged so the two questions cannot drift apart: an edge either
//! crosses a clock or it does not, and both callers have to agree about which.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use rtlscope_ir::{Design, ProcKind, Span};
use serde::{Deserialize, Serialize};

use crate::flat::{Flattened, SignalId};

/// What decides each signal, kept apart by whether a clock edge sits between.
///
/// Both maps run *backwards* — target to the signals that decide it — because
/// that is the direction the IR is written in: a process names what it assigns
/// and what that assignment reads. Walking forwards means inverting these,
/// which the depth query does once, for the slice it cares about.
pub struct SignalGraph {
    /// Registers: the value arrives on a clock edge.
    pub clocked: HashMap<SignalId, BTreeSet<SignalId>>,
    /// Wires: combinational logic, a latch, or a blocking assignment inside a
    /// clocked block. All three settle within one clock.
    pub combinational: HashMap<SignalId, BTreeSet<SignalId>>,
    /// Which clock ticks each register.
    pub clock_of: HashMap<SignalId, SignalId>,
    /// Registers whose load is guarded by something other than reset, and the
    /// conditions as the source wrote them.
    ///
    /// A clock enable is not a construct in the IR — it is an `if (en)` around
    /// an assignment, indistinguishable from any other condition. That is
    /// exactly what makes a latency variable, so it is recorded rather than
    /// resolved: the register still advances one stage structurally, but not
    /// every cycle.
    pub gated: HashMap<SignalId, Vec<String>>,
    /// Signals a `ProcKind::Latch` process writes.
    ///
    /// Kept so a loop verdict can leave them alone. A latch is storage that
    /// looks like a wire, so a cycle through one is not the combinational loop
    /// this module reports — [`crate::lint`] already has a finding for it.
    pub latchy: HashSet<SignalId>,
}

/// Reads a design into the graph both depth questions are asked of.
///
/// The fan-in is per assignment rather than per process, for the reason
/// [`crate::pipeline`] gives at length: one `always_ff` usually holds a whole
/// pipeline, and taking the process's own reads would make every register in it
/// depend on every other.
pub fn signal_graph(design: &Design, flat: &Flattened) -> SignalGraph {
    let mut clocked: HashMap<SignalId, BTreeSet<SignalId>> = HashMap::new();
    let mut combinational: HashMap<SignalId, BTreeSet<SignalId>> = HashMap::new();
    let mut clock_of: HashMap<SignalId, SignalId> = HashMap::new();
    let mut gated: HashMap<SignalId, Vec<String>> = HashMap::new();
    let mut latchy: HashSet<SignalId> = HashSet::new();

    for node in &flat.nodes {
        let module = node.module(design);
        for process in &module.procs {
            // What decides each assigned net, per assignment. The same query
            // the block diagram's drivers come from — see `crate::drive`.
            let fan_in = crate::drive::decides(module, process);

            match &process.kind {
                ProcKind::Ff { clk, rst, .. } => {
                    let Some(clock) = node.signal_of(clk) else { continue };
                    // The clock and the reset decide *when*, not *what*, and an
                    // edge from them would make every register in the domain
                    // depend on every other.
                    let timing: BTreeSet<SignalId> =
                        [node.signal_of(clk), rst.as_ref().and_then(|r| node.signal_of(&r.net))]
                            .into_iter()
                            .flatten()
                            .collect();

                    for (target, assigned) in fan_in {
                        let Some(target) = node.signal(target) else { continue };
                        let decides = assigned.sources();
                        let sources = decides
                            .iter()
                            .filter_map(|net| node.signal(*net))
                            .filter(|signal| !timing.contains(signal));

                        // `q <= d` inside a clocked block is a register. `t = d`
                        // is not: a blocking assignment settles within the same
                        // clock, so it is a wire with a name — which is what
                        // every temporary of an inlined function is. Counting
                        // those as registers would put a stage boundary inside
                        // one clock's worth of logic.
                        if assigned.blocking {
                            combinational.entry(target).or_default().extend(sources);
                        } else {
                            clock_of.insert(target, clock);
                            clocked.entry(target).or_default().extend(sources);

                            // A condition made only of timing nets is the reset
                            // mux, which every register has; it says nothing
                            // about when the data advances.
                            let guards: Vec<String> = assigned
                                .conditions
                                .iter()
                                .filter(|condition| {
                                    condition.nets.iter().any(|net| {
                                        node.signal(*net).is_none_or(|s| !timing.contains(&s))
                                    })
                                })
                                .map(|condition| condition.text.clone())
                                .collect();
                            if !guards.is_empty() {
                                gated.entry(target).or_default().extend(guards);
                            }
                        }
                    }
                }
                ProcKind::Comb | ProcKind::Latch => {
                    let is_latch = matches!(process.kind, ProcKind::Latch);
                    for (target, assigned) in fan_in {
                        let Some(target) = node.signal(target) else { continue };
                        if is_latch {
                            latchy.insert(target);
                        }
                        combinational
                            .entry(target)
                            .or_default()
                            .extend(assigned.sources().iter().filter_map(|net| node.signal(*net)));
                    }
                }
                // A power-on value is not driven by anything that changes.
                ProcKind::Initial => {}
            }
        }
    }

    // A register guarded in one place and not in another is not gated: some
    // path through it takes the data every cycle.
    gated.retain(|target, guards| {
        guards.sort_unstable();
        guards.dedup();
        clocked.contains_key(target)
    });

    SignalGraph { clocked, combinational, clock_of, gated, latchy }
}

/// Tarjan's strongly connected components, so that a cycle becomes one node.
///
/// Written out rather than pulled in: the graph is small, and an iterative
/// version avoids a stack overflow on the long chains a real design has.
///
/// Shared by the pipeline's register contraction and by combinational-loop
/// detection. The two run it over different edge sets and mean different things
/// by a group of more than one — a piece of state that advances together, and a
/// value that has no settled answer — but the cycle-finding is the same.
pub(crate) fn scc(
    members: &[SignalId],
    edges: &HashMap<SignalId, Vec<SignalId>>,
) -> Vec<Vec<SignalId>> {
    #[derive(Clone, Copy)]
    struct Mark {
        index: usize,
        low: usize,
        on_stack: bool,
    }

    let mut marks: HashMap<SignalId, Mark> = HashMap::new();
    let mut stack: Vec<SignalId> = Vec::new();
    let mut groups: Vec<Vec<SignalId>> = Vec::new();
    let mut next = 0usize;

    for root in members {
        if marks.contains_key(root) {
            continue;
        }
        // (node, how many of its edges have been taken)
        let mut work: Vec<(SignalId, usize)> = vec![(*root, 0)];
        marks.insert(*root, Mark { index: next, low: next, on_stack: true });
        next += 1;
        stack.push(*root);

        while let Some((node, taken)) = work.pop() {
            let empty = Vec::new();
            let out = edges.get(&node).unwrap_or(&empty);
            if taken < out.len() {
                work.push((node, taken + 1));
                let next_node = out[taken];
                match marks.get(&next_node).copied() {
                    None => {
                        marks.insert(next_node, Mark { index: next, low: next, on_stack: true });
                        next += 1;
                        stack.push(next_node);
                        work.push((next_node, 0));
                    }
                    Some(mark) if mark.on_stack => {
                        let low = marks[&node].low.min(mark.index);
                        marks.get_mut(&node).unwrap().low = low;
                    }
                    Some(_) => {}
                }
                continue;
            }

            // Every edge taken: fold this node's low-link into its parent's.
            let mark = marks[&node];
            if let Some((parent, _)) = work.last().copied() {
                let low = marks[&parent].low.min(mark.low);
                marks.get_mut(&parent).unwrap().low = low;
            }
            if mark.low == mark.index {
                let mut group = Vec::new();
                while let Some(member) = stack.pop() {
                    marks.get_mut(&member).unwrap().on_stack = false;
                    group.push(member);
                    if member == node {
                        break;
                    }
                }
                groups.push(group);
            }
        }
    }
    groups
}

/// Every cycle made only of wires.
///
/// A value that reaches itself without passing a clock edge has no settled
/// answer: synthesis cannot build it, and simulation either oscillates or picks
/// whichever order it happened to evaluate in. Groups come back with more than
/// one member, plus single signals that feed themselves directly.
///
/// Latch-written signals are left out. A latch is storage wearing a wire's
/// clothes, and its own feedback is how it holds — [`crate::lint`] reports the
/// latch itself, which is the finding a reader can act on.
pub(crate) fn combinational_loops(graph: &SignalGraph) -> Vec<Vec<SignalId>> {
    // Forward, and only between signals that are wires at both ends: an edge
    // into a register is a clock edge, and one out of a primary input starts
    // nowhere.
    let mut forward: HashMap<SignalId, Vec<SignalId>> = HashMap::new();
    for (target, sources) in &graph.combinational {
        if graph.latchy.contains(target) {
            continue;
        }
        for source in sources {
            if graph.combinational.contains_key(source) && !graph.latchy.contains(source) {
                forward.entry(*source).or_default().push(*target);
            }
        }
    }
    for out in forward.values_mut() {
        out.sort_unstable();
        out.dedup();
    }

    let mut members: Vec<SignalId> = graph
        .combinational
        .keys()
        .filter(|signal| !graph.latchy.contains(*signal))
        .copied()
        .collect();
    members.sort_unstable();

    let mut loops: Vec<Vec<SignalId>> = Vec::new();
    for mut group in scc(&members, &forward) {
        let one_that_feeds_itself = group.len() == 1
            && graph.combinational.get(&group[0]).is_some_and(|from| from.contains(&group[0]));
        if group.len() > 1 || one_that_feeds_itself {
            group.sort_unstable();
            loops.push(group);
        }
    }
    loops.sort();
    loops
}

/// The graph, walked the way a value travels.
///
/// Inverted per query rather than kept beside the backward maps: a forward copy
/// of the whole design would have to be invalidated in step with the other, and
/// only this question walks that way. The weight is what the edge costs — a
/// wire nothing, a clock edge one.
pub(crate) fn forward_edges(graph: &SignalGraph) -> HashMap<SignalId, Vec<(SignalId, u8)>> {
    let mut forward: HashMap<SignalId, Vec<(SignalId, u8)>> = HashMap::new();
    for (target, sources) in &graph.combinational {
        for source in sources {
            forward.entry(*source).or_default().push((*target, 0));
        }
    }
    for (target, sources) in &graph.clocked {
        for source in sources {
            forward.entry(*source).or_default().push((*target, 1));
        }
    }
    for out in forward.values_mut() {
        out.sort_unstable();
        out.dedup();
    }
    forward
}

/// Everything reachable from `start` over `edges`, `start` included.
pub(crate) fn reachable(
    start: SignalId,
    edges: &HashMap<SignalId, Vec<(SignalId, u8)>>,
) -> HashSet<SignalId> {
    let mut seen = HashSet::from([start]);
    let mut queue = VecDeque::from([start]);
    while let Some(signal) = queue.pop_front() {
        for (next, _) in edges.get(&signal).into_iter().flatten() {
            if seen.insert(*next) {
                queue.push_back(*next);
            }
        }
    }
    seen
}

// ------------------------------------------------------- from A to B ---

/// How many paths are worth showing.
///
/// Enumerating every one is exponential and nobody reads sixteen anyway; the
/// count and the two extremes are the answer, and the paths are there so the
/// reader can see *which* logic the numbers are about.
pub const MAX_PATHS: usize = 16;

/// How much walking the enumeration will do before it gives up and says so.
const MAX_WORK: usize = 65_536;

/// How many clocks lie between two signals, and along which logic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DepthReport {
    /// The names as the design knows them, which may be longer than what was
    /// asked for: `u_fir.acc` for an `acc`.
    pub from: String,
    pub to: String,
    /// The clock whose edges were counted. `None` when the path crosses no
    /// register at all — the two are one clock's worth of logic apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<String>,
    /// The fewest clocks the value can take. Exact whenever there is an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_stages: Option<usize>,
    /// The most, when that is a number at all. See [`DepthReport::feedback`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_stages: Option<usize>,
    /// Something on the way holds itself up: a counter, a state machine, an
    /// accumulator, a register that shifts into itself.
    ///
    /// Then there is no longest path — the walk can go round again — and the
    /// honest answer is that the maximum is not a structural fact. The dump is
    /// what answers it.
    pub feedback: bool,
    /// The value can arrive by paths of different length.
    pub reconvergent: bool,
    /// Something on the way does not advance every cycle.
    pub variable_latency: bool,
    /// Up to [`MAX_PATHS`] ways through, shortest first.
    pub paths: Vec<DepthPath>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<DepthError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<DepthWarning>,
    /// What the count could not account for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// One way from A to B, and the registers it clocks through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepthPath {
    pub stages: usize,
    /// In the order the value passes through them.
    pub registers: Vec<PathRegister>,
    /// Which of them do not advance every cycle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gated_at: Vec<String>,
}

/// A register on a path, and where to read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathRegister {
    pub name: String,
    pub signal: SignalId,
    /// Where it is declared, so a window can take the reader there.
    pub span: Span,
}

/// A reason there is no number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code")]
pub enum DepthError {
    /// The two are not on the same clock, so "how many clocks" has no answer.
    #[serde(rename = "E001")]
    CrossDomain { clocks: Vec<String>, registers: Vec<String> },
    /// A value on the way decides itself without passing a clock.
    #[serde(rename = "E002")]
    CombLoop { signals: Vec<String> },
    /// Nothing that leaves A arrives at B.
    #[serde(rename = "E003")]
    NoPath { from: String, to: String },
    /// A name the design does not have.
    #[serde(rename = "E004")]
    UnknownSignal { name: String, candidates: Vec<String> },
}

/// Something true about the answer that the numbers do not say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code")]
pub enum DepthWarning {
    /// Paths of different length reconverge.
    ///
    /// Usually a bug: one branch has a register the other does not, and the
    /// control signal no longer lines up with the data it was meant to escort.
    /// So the two numbers are both reported rather than averaged into one that
    /// is nowhere in the design.
    #[serde(rename = "W001")]
    Reconvergent { min: usize, max: usize },
    /// Registers on the way that are guarded by something other than reset.
    #[serde(rename = "W002")]
    Gated { registers: Vec<String>, guards: Vec<String> },
    /// The enumeration stopped before it ran out of paths.
    #[serde(rename = "W004")]
    Truncated { shown: usize, cap: usize },
}

impl DepthReport {
    /// Whether anything stopped this from being an answer.
    pub fn failed(&self) -> bool {
        !self.errors.is_empty()
    }

    fn refused(from: &str, to: &str, error: DepthError) -> DepthReport {
        DepthReport {
            from: from.to_string(),
            to: to.to_string(),
            clock: None,
            min_stages: None,
            max_stages: None,
            feedback: false,
            reconvergent: false,
            variable_latency: false,
            paths: Vec::new(),
            errors: vec![error],
            warnings: Vec::new(),
            problems: Vec::new(),
        }
    }
}

/// How many clocks from `from` to `to`.
///
/// Reads the design, builds the graph, asks. [`between`] is the same question
/// for a caller that already has both.
pub fn analyse(design: &Design, from: &str, to: &str) -> DepthReport {
    let flat = crate::flat::flatten(design);
    let graph = signal_graph(design, &flat);
    between(design, &flat, &graph, from, to)
}

/// The convention, stated once because everything else depends on it:
///
/// **the depth is the number of clock edges the value crosses on the way. The
/// walk starts at A's value, so A's own register does not count; arriving at a
/// register costs one, so B's does.**
///
/// On a three-deep pipeline that makes `in → out` three, `in → the first
/// register` one, and `the last register → out` nothing at all — the last
/// register's value *is* the output, one clock's worth of logic later.
pub fn between(
    design: &Design,
    flat: &Flattened,
    graph: &SignalGraph,
    from: &str,
    to: &str,
) -> DepthReport {
    let names = names_of(design, flat);
    let start = match resolve(&names, from) {
        Ok(signal) => signal,
        Err(error) => return DepthReport::refused(from, to, error),
    };
    let end = match resolve(&names, to) {
        Ok(signal) => signal,
        Err(error) => return DepthReport::refused(from, to, error),
    };
    let (from_name, to_name) = (flat.name_of(start), flat.name_of(end));

    let forward = forward_edges(graph);
    let backward = backward_edges(&forward);

    // Only the logic that both leaves A and arrives at B. Everything else in
    // the design is beside the question, and cutting it away first is what
    // keeps the loop and domain checks about *this* path rather than about the
    // design as a whole.
    let slice: HashSet<SignalId> =
        reachable(start, &forward).intersection(&reachable(end, &backward)).copied().collect();
    if !slice.contains(&end) || (start == end) {
        let error = DepthError::NoPath { from: from_name.clone(), to: to_name.clone() };
        return DepthReport::refused(&from_name, &to_name, error);
    }

    let mut report = DepthReport {
        from: from_name,
        to: to_name,
        clock: None,
        min_stages: None,
        max_stages: None,
        feedback: false,
        reconvergent: false,
        variable_latency: false,
        paths: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        problems: Vec::new(),
    };

    // A cycle of wires on the way means the value never settles, so counting
    // its clocks is counting something that does not happen.
    for group in combinational_loops(graph) {
        if group.iter().any(|signal| slice.contains(signal)) {
            let signals = group.iter().map(|s| flat.name_of(*s)).collect();
            report.errors.push(DepthError::CombLoop { signals });
            return report;
        }
    }

    // Which clocks tick the registers that would be counted. Asked of the slice
    // rather than of the whole design: a design may hold six clocks and still
    // have exactly one on the road between these two.
    // Two questions, and they take different sets.
    //
    // Whether the road crosses domains has to include A's own register: a value
    // that leaves a register on one clock and lands on another has crossed,
    // even with nothing in between, and that is exactly the case a check over
    // the arrivals alone would wave through.
    //
    // Which clock was *counted* is the other question, and there A's register
    // is not an answer — the walk starts at its output. That is what makes
    // `the last register -> the output it drives` zero clocks on no clock at
    // all, rather than zero clocks on the one it happens to sit on.
    let mut clocks: BTreeMap<SignalId, Vec<SignalId>> = BTreeMap::new();
    for signal in &slice {
        if let Some(clock) = graph.clock_of.get(signal) {
            clocks.entry(*clock).or_default().push(*signal);
        }
    }
    if clocks.len() > 1 {
        let names = clocks.keys().map(|c| flat.name_of(*c)).collect();
        let registers = clocks
            .values()
            .filter_map(|members| members.iter().min())
            .map(|s| flat.name_of(*s))
            .collect();
        report.errors.push(DepthError::CrossDomain { clocks: names, registers });
        return report;
    }
    report.clock = slice
        .iter()
        .filter(|signal| **signal != start)
        .find_map(|signal| graph.clock_of.get(signal))
        .map(|clock| flat.name_of(*clock));

    let within: HashMap<SignalId, Vec<(SignalId, u8)>> = slice
        .iter()
        .map(|signal| {
            let out = forward
                .get(signal)
                .map(|edges| {
                    edges.iter().filter(|(next, _)| slice.contains(next)).copied().collect()
                })
                .unwrap_or_default();
            (*signal, out)
        })
        .collect();

    report.min_stages = Some(shortest(start, end, &within));

    // The longest path is a number only when the road does not loop back on
    // itself. Through a counter or a state machine the walk can go round again,
    // and printing the longest walk that happens not to repeat a signal would
    // be printing an arbitrary number — the same judgment `crate::pipeline`
    // makes about feedback. The dump answers this one.
    match longest(start, end, &within, &slice) {
        Some(max) => {
            report.max_stages = Some(max);
            let min = report.min_stages.unwrap_or(max);
            if min != max {
                report.reconvergent = true;
                report.warnings.push(DepthWarning::Reconvergent { min, max });
            }
        }
        None => {
            report.feedback = true;
            report.variable_latency = true;
            let looping = self_looping(&slice, graph);
            for name in looping.iter().map(|s| flat.name_of(*s)) {
                // Whether this is a counter, whose bits are beside the
                // point, or a shift register, whose bits are the whole
                // answer, is exactly what counting whole signals cannot
                // tell. So the note states the fact and leaves the reader
                // the judgment.
                report.problems.push(format!(
                    "`{name}` feeds itself, so the walk can go round again; if it shifts \
                     through itself, the depth is a count of bits that RTLScope does not take"
                ));
            }
            if looping.is_empty() {
                report.problems.push(
                    "something on the way holds itself up, so there is no longest path".to_string(),
                );
            }
        }
    }

    // Gating is read from every register on the road, not only from the paths
    // that fit in the report: a truncated enumeration must not change whether
    // the latency is called variable.
    let mut gated: Vec<String> = Vec::new();
    let mut guards: Vec<String> = Vec::new();
    for signal in slice.iter().filter(|s| **s != start && graph.clock_of.contains_key(*s)) {
        if let Some(reasons) = graph.gated.get(signal) {
            gated.push(flat.name_of(*signal));
            guards.extend(reasons.iter().cloned());
        }
    }
    gated.sort();
    guards.sort();
    guards.dedup();
    if !gated.is_empty() {
        report.variable_latency = true;
        report.warnings.push(DepthWarning::Gated { registers: gated, guards });
    }

    let (paths, truncated) = enumerate(start, end, &within);
    report.paths = paths
        .into_iter()
        .map(|registers| {
            let gated_at = registers
                .iter()
                .filter(|signal| graph.gated.contains_key(*signal))
                .map(|s| flat.name_of(*s))
                .collect();
            DepthPath {
                stages: registers.len(),
                registers: registers
                    .iter()
                    .map(|signal| PathRegister {
                        name: flat.name_of(*signal),
                        signal: *signal,
                        span: span_of(design, flat, *signal),
                    })
                    .collect(),
                gated_at,
            }
        })
        .collect();
    if truncated {
        report.warnings.push(DepthWarning::Truncated { shown: report.paths.len(), cap: MAX_PATHS });
    }

    report
}

/// Every name the design knows a signal by, for resolving what was asked for.
fn names_of(design: &Design, flat: &Flattened) -> BTreeMap<String, SignalId> {
    flat.all_names(design)
        .filter(|(_, _, _, invented)| !invented)
        .map(|(name, signal, _, _)| (name, signal))
        .collect()
}

/// The signal a reader meant.
///
/// The full path first, then the leaf name — somebody looking at a waveform
/// types `acc`, not `u_dsp.u_fir.acc`. An ambiguous leaf is refused rather than
/// guessed: picking one of four `acc`s silently is how a reader ends up reading
/// an answer about a different instance.
fn resolve(names: &BTreeMap<String, SignalId>, want: &str) -> Result<SignalId, DepthError> {
    let want = want.trim();
    if let Some(signal) = names.get(want) {
        return Ok(*signal);
    }
    let leaves: Vec<(&String, &SignalId)> =
        names.iter().filter(|(name, _)| name.rsplit('.').next() == Some(want)).collect();
    match leaves.as_slice() {
        [(_, signal)] => Ok(**signal),
        [] => Err(DepthError::UnknownSignal {
            name: want.to_string(),
            candidates: nearby(names, want),
        }),
        several => Err(DepthError::UnknownSignal {
            name: want.to_string(),
            candidates: several.iter().map(|(name, _)| (*name).clone()).take(8).collect(),
        }),
    }
}

/// A few names worth suggesting to somebody who typed one that is not there.
fn nearby(names: &BTreeMap<String, SignalId>, want: &str) -> Vec<String> {
    let lower = want.to_lowercase();
    let mut close: Vec<String> =
        names.keys().filter(|name| name.to_lowercase().contains(&lower)).take(8).cloned().collect();
    if close.is_empty() {
        close = names.keys().take(8).cloned().collect();
    }
    close
}

/// Where a signal is declared.
fn span_of(design: &Design, flat: &Flattened, signal: SignalId) -> Span {
    crate::drive::nets_of(design, flat, signal)
        .first()
        .map(|(module, net)| design.module(*module).net(*net).span)
        // A signal with no home in the design is one RTLScope invented, and the
        // resolver already refuses those — but a span it cannot point at is a
        // fact about the answer, not a reason to panic in the middle of one.
        .unwrap_or(Span::UNKNOWN)
}

/// The graph walked backwards, for finding what can reach B.
fn backward_edges(
    forward: &HashMap<SignalId, Vec<(SignalId, u8)>>,
) -> HashMap<SignalId, Vec<(SignalId, u8)>> {
    let mut backward: HashMap<SignalId, Vec<(SignalId, u8)>> = HashMap::new();
    for (source, edges) in forward {
        for (target, weight) in edges {
            backward.entry(*target).or_default().push((*source, *weight));
        }
    }
    backward
}

/// The fewest clock edges, by 0-1 BFS.
///
/// A wire costs nothing and a register costs one, which is exactly the shape a
/// deque handles in one pass: a free step goes to the front of the queue and a
/// paid one to the back, so the queue stays sorted by cost without a heap.
fn shortest(
    start: SignalId,
    end: SignalId,
    edges: &HashMap<SignalId, Vec<(SignalId, u8)>>,
) -> usize {
    let mut cost: HashMap<SignalId, usize> = HashMap::from([(start, 0)]);
    let mut queue: VecDeque<SignalId> = VecDeque::from([start]);
    while let Some(signal) = queue.pop_front() {
        let here = cost[&signal];
        for (next, weight) in edges.get(&signal).into_iter().flatten() {
            let then = here + *weight as usize;
            if cost.get(next).is_none_or(|known| then < *known) {
                cost.insert(*next, then);
                match weight {
                    0 => queue.push_front(*next),
                    _ => queue.push_back(*next),
                }
            }
        }
    }
    cost.get(&end).copied().unwrap_or(0)
}

/// The most clock edges, or `None` when the road loops.
///
/// Kahn's ordering does both jobs at once: it produces the order the longest
/// path needs, and it fails to consume every signal exactly when there is a
/// cycle — which is the case where the question has no answer.
fn longest(
    start: SignalId,
    end: SignalId,
    edges: &HashMap<SignalId, Vec<(SignalId, u8)>>,
    slice: &HashSet<SignalId>,
) -> Option<usize> {
    let mut incoming: HashMap<SignalId, usize> = slice.iter().map(|s| (*s, 0)).collect();
    for out in edges.values() {
        for (target, _) in out {
            *incoming.entry(*target).or_insert(0) += 1;
        }
    }

    let mut ready: VecDeque<SignalId> =
        incoming.iter().filter(|(_, n)| **n == 0).map(|(s, _)| *s).collect();
    let mut order: Vec<SignalId> = Vec::new();
    while let Some(signal) = ready.pop_front() {
        order.push(signal);
        for (next, _) in edges.get(&signal).into_iter().flatten() {
            let left = incoming.get_mut(next).expect("every target is in the slice");
            *left -= 1;
            if *left == 0 {
                ready.push_back(*next);
            }
        }
    }
    if order.len() != slice.len() {
        return None;
    }

    // Reachable-from-A only: a signal the walk cannot get to must not seed a
    // path with a cost of zero and then hand it on.
    let mut best: HashMap<SignalId, usize> = HashMap::from([(start, 0)]);
    for signal in order {
        let Some(here) = best.get(&signal).copied() else { continue };
        for (next, weight) in edges.get(&signal).into_iter().flatten() {
            let then = here + *weight as usize;
            let known = best.entry(*next).or_insert(then);
            *known = (*known).max(then);
        }
    }
    best.get(&end).copied()
}

/// Registers in the slice that are their own source.
fn self_looping(slice: &HashSet<SignalId>, graph: &SignalGraph) -> Vec<SignalId> {
    let mut out: Vec<SignalId> = slice
        .iter()
        .filter(|signal| {
            graph.clocked.get(*signal).is_some_and(|sources| sources.contains(*signal))
        })
        .copied()
        .collect();
    out.sort_unstable();
    out
}

/// Ways through, as the registers each one clocks into.
///
/// Simple paths only — a walk that revisits a signal is going round a loop, and
/// the loop is already reported as feedback. Bounded twice over: by how many
/// paths are worth showing, and by how much walking is worth doing to find
/// them. Both bounds are reported when they bite, because a list that quietly
/// stopped early reads as the whole answer.
fn enumerate(
    start: SignalId,
    end: SignalId,
    edges: &HashMap<SignalId, Vec<(SignalId, u8)>>,
) -> (Vec<Vec<SignalId>>, bool) {
    let mut found: Vec<Vec<SignalId>> = Vec::new();
    let mut truncated = false;
    let mut work = 0usize;

    // (signal, registers passed so far, signals on this path)
    let mut stack: Vec<(SignalId, Vec<SignalId>, HashSet<SignalId>)> =
        vec![(start, Vec::new(), HashSet::from([start]))];

    while let Some((signal, registers, seen)) = stack.pop() {
        work += 1;
        if work > MAX_WORK || found.len() >= MAX_PATHS {
            truncated = true;
            break;
        }
        // A road with no registers on it is still a road: `the last register
        // -> the output it drives` is zero stages, and showing it as a path
        // with nothing on it is how the report says so. `start == end` was
        // refused before the walk began.
        if signal == end {
            found.push(registers);
            continue;
        }
        for (next, weight) in edges.get(&signal).into_iter().flatten() {
            if seen.contains(next) {
                continue;
            }
            let mut onwards = registers.clone();
            if *weight == 1 {
                onwards.push(*next);
            }
            let mut walked = seen.clone();
            walked.insert(*next);
            stack.push((*next, onwards, walked));
        }
    }

    found.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    found.dedup();
    found.truncate(MAX_PATHS);
    (found, truncated)
}

#[cfg(test)]
mod tests {
    use rtlscope_sv::ParseOptions;

    use super::*;
    use crate::flat::flatten;

    fn design(fixture: &str, top: Option<&str>) -> Design {
        let path = rtlscope_fixtures::path(fixture);
        let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
        rtlscope_elab::elaborate(&uir, top).0.expect("elaborates")
    }

    /// The split the whole module rests on: an edge either crosses a clock or
    /// it does not. A chain of three registers has three clocked targets and
    /// none of them reads another through a clocked edge of its own.
    #[test]
    fn a_register_chain_is_three_clocked_targets_and_no_more() {
        let design = design("pipeline3.sv", Some("pipeline3"));
        let flat = flatten(&design);
        let graph = signal_graph(&design, &flat);

        let named = |signal: &SignalId| flat.name_of(*signal);
        let mut registers: Vec<String> = graph.clocked.keys().map(named).collect();
        registers.sort();
        assert!(
            registers.iter().any(|name| name == "data_d1")
                && registers.iter().any(|name| name == "data_d3"),
            "the data path registers are clocked: {registers:?}"
        );
        assert!(
            !registers.iter().any(|name| name == "clk"),
            "a clock is not a register it ticks: {registers:?}"
        );
        assert!(graph.latchy.is_empty(), "nothing here is a latch");
        assert!(combinational_loops(&graph).is_empty(), "and nothing feeds itself through wires");
    }

    /// A reset is not a gate. Every register has one and it says nothing about
    /// when the data advances, so a design of plain registers must come back
    /// with nothing gated at all.
    #[test]
    fn a_reset_is_not_what_makes_a_latency_variable() {
        let design = design("pipeline3.sv", Some("pipeline3"));
        let flat = flatten(&design);
        let graph = signal_graph(&design, &flat);

        let gated: Vec<String> = graph.gated.keys().map(|s| flat.name_of(*s)).collect();
        assert!(gated.is_empty(), "a plain pipeline gates nothing: {gated:?}");
    }

    /// And a real gate is. `staged` in the sample only takes `in_data` when
    /// `gate` says so, which is the whole reason its latency is not fixed.
    #[test]
    fn a_condition_that_is_not_the_reset_is_recorded_as_a_gate() {
        let design = design("trace_demo.sv", Some("trace_demo"));
        let flat = flatten(&design);
        let graph = signal_graph(&design, &flat);

        let staged = graph
            .gated
            .iter()
            .find(|(signal, _)| flat.name_of(**signal) == "staged")
            .map(|(_, guards)| guards.clone());
        assert!(staged.is_some(), "`staged` is guarded: {:?}", graph.gated.len());
        let guards = staged.expect("just checked");
        assert!(
            guards.iter().any(|text| text.contains("gate")),
            "and the guard is named as the source wrote it: {guards:?}"
        );
    }

    /// The finding this module adds to the crate. A cycle of wires is a real
    /// bug — synthesis cannot build it — and the fixture carries an innocent
    /// chain beside it that must not be swept up.
    #[test]
    fn a_cycle_of_wires_is_found_and_a_plain_chain_is_not() {
        let design = design("comb_loop.sv", Some("comb_loop"));
        let flat = flatten(&design);
        let graph = signal_graph(&design, &flat);

        let loops = combinational_loops(&graph);
        assert_eq!(loops.len(), 1, "one loop: {loops:?}");
        let mut names: Vec<String> = loops[0].iter().map(|s| flat.name_of(*s)).collect();
        names.sort();
        assert_eq!(names, ["knot_a", "knot_b"], "the two that hold each other up");
    }
}
