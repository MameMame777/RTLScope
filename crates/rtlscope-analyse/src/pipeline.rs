//! How many clocks deep the logic is, and which registers sit at each depth.
//!
//! A pipeline is not a construct anyone writes down: it is a shape the
//! registers fall into. `d1 <= in; d2 <= d1; d3 <= d2;` is three stages because
//! of what feeds what, and the same three lines scattered across three modules
//! are still three stages. So the question is asked of the register adjacency
//! graph — an edge from A to B when the value in A reaches B in one clock,
//! through however much combinational logic — and the answer is how long the
//! longest path through it is.
//!
//! Two things make this more than a graph walk.
//!
//! **The fan-in has to be per assignment, not per process.** One `always_ff`
//! usually holds a whole pipeline, and the process reads everything every stage
//! reads. Taking the process's reads would make every register depend on every
//! other and collapse the pipeline to one stage. So each assignment is walked
//! for what decides *its* next value: the right-hand side, plus the conditions
//! on the way to it, since `if (en) q <= d;` depends on `en` as much as on `d`.
//!
//! **Feedback is not a mistake.** A counter feeds itself, an accumulator feeds
//! itself, a state machine feeds itself; a graph with cycles has no longest
//! path. Each cycle is contracted to a single node — which is what it is, one
//! piece of state that advances together — and reported as such rather than
//! being given a stage number that would mean nothing.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rtlscope_ir::Design;
use serde::{Deserialize, Serialize};

use crate::flat::{SignalId, flatten};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineReport {
    /// One per clock domain that has any registers at all, deepest first.
    pub domains: Vec<DomainDepth>,
    pub registers: usize,
    /// Which stage each register sits at, by the net that holds it in the
    /// module that declares it.
    ///
    /// For the block diagram, which draws boxes rather than reads names: a
    /// process whose registers span several stages is drawn as one box per
    /// stage, and this is how it finds out. A net that is a register in two
    /// instances of one module at different depths gets the shallower stage —
    /// one answer per module, since the diagram is of the module.
    ///
    /// Not serialised. The reports that go out as JSON say the same thing by
    /// name in `stages`, and a net id means nothing outside the process that
    /// elaborated it.
    #[serde(skip)]
    pub stage_by_net: HashMap<(rtlscope_ir::ModuleId, rtlscope_ir::NetId), usize>,
    /// What each register reads directly, in the same terms: the nets its
    /// next value is computed from, within the module that declares it.
    ///
    /// This is what lets a split box put each read pin on the stage that
    /// reads it, rather than on all of them.
    /// The registers that hold each other up — counters, pointers, state —
    /// in the same terms. A box cut into stages is a claim that values pass
    /// through it one clock at a time, and these are the registers for which
    /// that is not what is happening.
    #[serde(skip)]
    pub feedback_by_net: HashSet<(rtlscope_ir::ModuleId, rtlscope_ir::NetId)>,
    #[serde(skip)]
    pub feeds_by_net: HashMap<
        (rtlscope_ir::ModuleId, rtlscope_ir::NetId),
        BTreeSet<(rtlscope_ir::ModuleId, rtlscope_ir::NetId)>,
    >,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainDepth {
    pub clock: String,
    /// How many clocks it takes for a value entering this domain to reach the
    /// far end of the deepest path.
    pub depth: usize,
    pub stages: Vec<Stage>,
    /// Groups of registers that hold each other up: counters, accumulators,
    /// state machines. They advance together and share the stage they sit at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<Feedback>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    /// Clocks from an input of this domain. Stage 0 is fed from outside it.
    pub index: usize,
    pub registers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    pub stage: usize,
    pub registers: Vec<String>,
}

/// How long a chain of registers may be before this gives up.
///
/// A real pipeline is tens of stages at most; anything longer means the graph
/// has a cycle the contraction did not catch, and stopping beats looping.
const MAX_DEPTH: usize = 4096;

pub fn analyse(design: &Design) -> PipelineReport {
    let flat = flatten(design);

    // The graph both depth questions are asked of. Built next door so that
    // this one and `crate::depth`'s cannot come to disagree about which edges
    // cross a clock — see `crate::depth::signal_graph`.
    let graph = crate::depth::signal_graph(design, &flat);
    let crate::depth::SignalGraph { clocked, combinational, clock_of, .. } = &graph;

    // Register to register, with the combinational logic between them walked
    // through rather than counted.
    let mut edges: HashMap<SignalId, BTreeSet<SignalId>> = HashMap::new();
    for (target, sources) in clocked {
        let mut reached = BTreeSet::new();
        for source in sources {
            walk_back(*source, clocked, combinational, &mut HashSet::new(), &mut reached);
        }
        // A register that feeds itself stays an edge: `count <= count + 1` is
        // feedback, and dropping the self-edge would call a counter a plain
        // one-stage register.
        edges.insert(*target, reached);
    }

    let mut by_domain: BTreeMap<SignalId, Vec<SignalId>> = BTreeMap::new();
    for (register, clock) in clock_of {
        by_domain.entry(*clock).or_default().push(*register);
    }

    let registers = clock_of.len();
    let mut stage_by_signal: HashMap<SignalId, usize> = HashMap::new();
    let mut feedback_by_signal: HashSet<SignalId> = HashSet::new();
    let mut domains: Vec<DomainDepth> = by_domain
        .into_iter()
        .map(|(clock, mut members)| {
            members.sort_unstable();
            let (domain, stages, looping) = layer(&flat, clock, &members, &edges, clock_of);
            stage_by_signal.extend(stages);
            feedback_by_signal.extend(looping);
            domain
        })
        .collect();
    domains.sort_by(|a, b| b.depth.cmp(&a.depth).then_with(|| a.clock.cmp(&b.clock)));

    // Back from wires to the nets that hold them, one answer per module. A
    // wire has a name in every module it crosses; only the one that *declares*
    // the register is where the register is drawn, and that is the home whose
    // net the process writes — every home is kept, and the shallowest stage
    // wins where two instances disagree.
    let nets_of = |signal: SignalId| -> Vec<(rtlscope_ir::ModuleId, rtlscope_ir::NetId)> {
        flat.homes(signal).iter().map(|(node, net)| (flat.nodes[*node].module, *net)).collect()
    };
    let mut stage_by_net = HashMap::new();
    let mut feeds_by_net: HashMap<_, BTreeSet<_>> = HashMap::new();
    for (signal, stage) in &stage_by_signal {
        for key in nets_of(*signal) {
            let slot = stage_by_net.entry(key).or_insert(*stage);
            *slot = (*slot).min(*stage);
        }
        if let Some(sources) = clocked.get(signal) {
            for key in nets_of(*signal) {
                let into = feeds_by_net.entry(key).or_default();
                for source in sources {
                    // Only reads in the same module count: a pin is a net of
                    // the module being drawn, and a source that lives elsewhere
                    // reaches this one through a port that is a net here too.
                    into.extend(
                        nets_of(*source).into_iter().filter(|(module, _)| *module == key.0),
                    );
                }
            }
        }
    }

    let feedback_by_net: HashSet<_> =
        feedback_by_signal.iter().flat_map(|signal| nets_of(*signal)).collect();

    PipelineReport { domains, registers, stage_by_net, feedback_by_net, feeds_by_net }
}

/// Follows a signal back until it reaches registers or the edge of the design.
///
/// Combinational logic is transparent here: it takes no clock, so whatever
/// feeds it feeds whatever it feeds.
fn walk_back(
    signal: SignalId,
    clocked: &HashMap<SignalId, BTreeSet<SignalId>>,
    combinational: &HashMap<SignalId, BTreeSet<SignalId>>,
    seen: &mut HashSet<SignalId>,
    out: &mut BTreeSet<SignalId>,
) {
    if !seen.insert(signal) {
        return;
    }
    if clocked.contains_key(&signal) {
        out.insert(signal);
        return;
    }
    // Nothing driving it means a primary input, or a signal nobody drives:
    // either way the chain starts here.
    if let Some(sources) = combinational.get(&signal) {
        for source in sources {
            walk_back(*source, clocked, combinational, seen, out);
        }
    }
}

/// Assigns every register in one domain a stage.
///
/// Cycles are contracted first — a counter is one piece of state, not an
/// infinite chain — and then the longest path to each contracted node is its
/// stage.
fn layer(
    flat: &crate::flat::Flattened,
    clock: SignalId,
    members: &[SignalId],
    edges: &HashMap<SignalId, BTreeSet<SignalId>>,
    clock_of: &HashMap<SignalId, SignalId>,
) -> (DomainDepth, HashMap<SignalId, usize>, HashSet<SignalId>) {
    let inside: HashSet<SignalId> = members.iter().copied().collect();

    // Only edges that stay in this domain: one that leaves it is a clock
    // crossing, and counting it as a stage would be a claim about timing that
    // does not hold.
    let local: HashMap<SignalId, Vec<SignalId>> = members
        .iter()
        .map(|register| {
            let sources = edges
                .get(register)
                .map(|set| {
                    set.iter()
                        .copied()
                        .filter(|s| inside.contains(s) && clock_of.get(s) == Some(&clock))
                        .collect()
                })
                .unwrap_or_default();
            (*register, sources)
        })
        .collect();

    let groups = crate::depth::scc(members, &local);
    let group_of: HashMap<SignalId, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(index, group)| group.iter().map(move |r| (*r, index)))
        .collect();

    // Between contracted groups, which is a DAG.
    let mut incoming: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); groups.len()];
    for (register, sources) in &local {
        let to = group_of[register];
        for source in sources {
            let from = group_of[source];
            if from != to {
                incoming[to].insert(from);
            }
        }
    }

    let stage_of = longest_paths(&incoming);
    let depth = stage_of.iter().copied().max().map_or(0, |deepest| deepest + 1);

    let mut stages: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut feedback = Vec::new();
    let mut looping: HashSet<SignalId> = HashSet::new();
    for (index, group) in groups.iter().enumerate() {
        // Two distinct wires can share a shallowest name — the same net in
        // two instances of one module — and listing it twice reads as a bug.
        let mut names: Vec<String> = group.iter().map(|r| flat.name_of(*r)).collect();
        names.sort();
        names.dedup();
        stages.entry(stage_of[index]).or_default().extend(names.iter().cloned());
        // A cycle of two or more, or a register that feeds itself: either way
        // it is state that advances rather than a value passing through.
        let loops = group.len() > 1
            || group
                .first()
                .is_some_and(|only| local.get(only).is_some_and(|sources| sources.contains(only)));
        if loops {
            looping.extend(group.iter().copied());
            feedback.push(Feedback { stage: stage_of[index], registers: names });
        }
    }

    // Per register rather than per name, for a caller that draws rather than
    // reads: two wires with one shallowest name are still two wires.
    let by_signal: HashMap<SignalId, usize> =
        members.iter().map(|register| (*register, stage_of[group_of[register]])).collect();

    let domain = DomainDepth {
        clock: flat.name_of(clock),
        depth,
        stages: stages
            .into_iter()
            .map(|(index, mut registers)| {
                registers.sort();
                registers.dedup();
                Stage { index, registers }
            })
            .collect(),
        feedback,
    };
    (domain, by_signal, looping)
}

/// The longest path to each node of a DAG, given each node's predecessors.
fn longest_paths(incoming: &[BTreeSet<usize>]) -> Vec<usize> {
    let mut stage = vec![0usize; incoming.len()];
    // Repeated relaxation. Tarjan emits components in reverse topological order,
    // so one pass would nearly do; the loop is what makes that not matter.
    for _ in 0..incoming.len().min(MAX_DEPTH) {
        let mut changed = false;
        for (node, sources) in incoming.iter().enumerate() {
            let deepest = sources.iter().map(|s| stage[*s] + 1).max().unwrap_or(0);
            if deepest > stage[node] {
                stage[node] = deepest;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    stage
}
