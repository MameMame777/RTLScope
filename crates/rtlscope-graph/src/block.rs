//! The block graph of one module: what drives what.
//!
//! Nodes are the things a block diagram draws as boxes — the module's own
//! ports, its child instances, and its processes. Edges are nets, one per
//! driver/load pair, so a net feeding three places becomes three edges that
//! share a [`NetId`] and can be highlighted together.
//!
//! Clock and reset nets are marked rather than removed. Drawing them makes a
//! diagram unreadable — every flop connects to the clock — but deleting them
//! would hide real structure, so the renderer hides them by default and can be
//! told to show them.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use petgraph::stable_graph::{NodeIndex, StableDiGraph};
use rtlscope_analyse::drive;
use rtlscope_ir::{
    Conn, Design, InstId, Module, ModuleId, NetId, NetRef, PortDir, PortId, ProcId, ProcKind, Span,
};

/// One box in the diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockNode {
    /// An input of the module being drawn, on the left edge.
    InPort(PortId),
    /// An output, on the right edge.
    OutPort(PortId),
    Inst(InstId),
    Proc(ProcId),
    /// One stage of a process whose registers span several.
    ///
    /// A three-deep pipeline written as one `always_ff` is one process, and
    /// drawn as one box it is a box with `d1`, `d2` and `d3` on both sides and
    /// no wire between them — the wire from the box to itself is the one edge
    /// the graph drops. Cut by stage, the chain is three boxes and two wires,
    /// which is what the code says and what a reader is looking for.
    Stage {
        proc: ProcId,
        stage: u32,
    },
}

/// What a wire carries, which decides whether it is drawn by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WireKind {
    Signal,
    Clock,
    Reset,
}

#[derive(Debug, Clone)]
pub struct BlockEdge {
    pub net: NetId,
    /// The pin this edge leaves from, named as the source box names it.
    pub from_pin: String,
    /// The pin it arrives at.
    pub to_pin: String,
    pub kind: WireKind,
}

pub type BlockGraph = StableDiGraph<BlockNode, BlockEdge>;

/// A net a box drives and reads back: a register that advances, a comb loop.
///
/// Kept out of the graph, because a layered layout ranks by following edges
/// and an edge from a node to itself is a cycle of length one — but not kept
/// out of the picture. Drawn as one box with `count` on both sides and nothing
/// between them, a counter could not be told from a register fed by something
/// that was not drawn, and that is the one thing about it a reader wants to
/// know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loop {
    pub node: BlockNode,
    pub net: NetId,
    pub from_pin: String,
    pub to_pin: String,
    pub kind: WireKind,
}

pub struct Block {
    pub graph: BlockGraph,
    pub index: HashMap<BlockNode, NodeIndex>,
    /// The processes drawn as several boxes, and how each was cut.
    pub splits: HashMap<ProcId, Split>,
    /// The nets that leave a box and come straight back to it.
    pub loops: Vec<Loop>,
}

/// How one process is cut into stages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Split {
    /// The stages, lowest first, each with the nets the process writes there.
    pub writes: BTreeMap<u32, BTreeSet<NetId>>,
    /// For each net the process reads, the stages whose registers read it.
    ///
    /// A net not in here is read by no register in particular — a clock, a
    /// reset — and is put on every stage, since every stage takes it.
    pub reads: HashMap<NetId, BTreeSet<u32>>,
}

impl Split {
    /// The stages a read pin belongs on.
    pub fn stages_reading(&self, net: NetId) -> Vec<u32> {
        match self.reads.get(&net) {
            Some(stages) => stages.iter().copied().collect(),
            None => self.writes.keys().copied().collect(),
        }
    }

    /// The stage a written net belongs to.
    pub fn stage_writing(&self, net: NetId) -> Option<u32> {
        self.writes.iter().find(|(_, nets)| nets.contains(&net)).map(|(stage, _)| *stage)
    }
}

/// Where every register of a design sits in its pipeline, worked out once.
///
/// Once for the design rather than once per box, like [`ClockPorts`]: the
/// answer comes from flattening the whole design and walking its register
/// graph, and a window drawing forty modules should not do that forty times.
#[derive(Debug, Clone, Default)]
pub struct Stages {
    stage: HashMap<(ModuleId, NetId), usize>,
    feeds: HashMap<(ModuleId, NetId), BTreeSet<(ModuleId, NetId)>>,
    feedback: HashSet<(ModuleId, NetId)>,
}

impl Stages {
    pub fn of(design: &Design) -> Self {
        let report = rtlscope_analyse::pipeline::analyse(design);
        Stages {
            stage: report.stage_by_net,
            feeds: report.feeds_by_net,
            feedback: report.feedback_by_net,
        }
    }

    /// No stages at all, so every process is one box. For a caller that
    /// wants the older drawing, or a test that is not about pipelines.
    pub fn none() -> Self {
        Stages::default()
    }
}

/// How a process is cut, if it is.
///
/// Only a clocked process whose registers land in two or more stages, and
/// only when *every* register it writes has a stage: a box labelled "stage 2"
/// that quietly holds a register nobody measured a stage for would be the
/// diagram claiming something the analysis never said.
///
/// And only when none of them feeds itself. A cut is a claim that values pass
/// through, one clock a box; a counter, a pointer or a state register is state
/// that *advances*, and a FIFO's control process drawn as "stage 0" and
/// "stage 1" would be the diagram calling it a pipeline. Measured on
/// `fifo.sv`, which the first version of this cut in three.
fn split_of(module_id: ModuleId, process: &rtlscope_ir::Process, stages: &Stages) -> Option<Split> {
    if !matches!(process.kind, ProcKind::Ff { .. }) {
        return None;
    }
    if process
        .writes
        .iter()
        .filter_map(NetRef::net_id)
        .any(|net| stages.feedback.contains(&(module_id, net)))
    {
        return None;
    }
    let mut writes: BTreeMap<u32, BTreeSet<NetId>> = BTreeMap::new();
    for net in process.writes.iter().filter_map(NetRef::net_id) {
        let stage = *stages.stage.get(&(module_id, net))?;
        writes.entry(stage as u32).or_default().insert(net);
    }
    if writes.len() < 2 {
        return None;
    }
    let mut reads: HashMap<NetId, BTreeSet<u32>> = HashMap::new();
    for (stage, nets) in &writes {
        for net in nets {
            let Some(sources) = stages.feeds.get(&(module_id, *net)) else { continue };
            for (_, source) in sources {
                reads.entry(*source).or_default().insert(*stage);
            }
        }
    }
    Some(Split { writes, reads })
}

impl Block {
    pub fn node(&self, node: BlockNode) -> Option<NodeIndex> {
        self.index.get(&node).copied()
    }
}

/// Builds the block graph for one elaborated module.
pub fn build(design: &Design, module_id: ModuleId) -> Block {
    build_with(design, module_id, &ClockPorts::of(design), &Stages::of(design))
}

/// Builds a block graph reusing the tables computed once for the design.
///
/// Drawing several modules — as the GUI does while the user walks the
/// hierarchy — should not recompute them each time.
pub fn build_with(
    design: &Design,
    module_id: ModuleId,
    clock_ports: &ClockPorts,
    stages: &Stages,
) -> Block {
    let module = design.module(module_id);
    let clocks = ClockNets::of(design, module_id, clock_ports);

    let mut graph = BlockGraph::new();
    let mut index = HashMap::new();

    let add = |graph: &mut BlockGraph, index: &mut HashMap<_, _>, node: BlockNode| {
        *index.entry(node).or_insert_with(|| graph.add_node(node))
    };

    // Ports first, so the left and right edges of the diagram are stable.
    for (position, port) in module.ports.iter().enumerate() {
        let id = PortId(position as u32);
        let node = match port.dir {
            PortDir::Input => BlockNode::InPort(id),
            // An `inout` is drawn on the right; there is nowhere better, and the
            // diagram marks the direction on the pin.
            PortDir::Output | PortDir::Inout => BlockNode::OutPort(id),
        };
        add(&mut graph, &mut index, node);
    }
    for position in 0..module.insts.len() {
        add(&mut graph, &mut index, BlockNode::Inst(InstId(position as u32)));
    }
    let mut splits: HashMap<ProcId, Split> = HashMap::new();
    for (position, process) in module.procs.iter().enumerate() {
        let id = ProcId(position as u32);
        match split_of(module_id, process, stages) {
            Some(split) => {
                for stage in split.writes.keys() {
                    add(&mut graph, &mut index, BlockNode::Stage { proc: id, stage: *stage });
                }
                splits.insert(id, split);
            }
            None => {
                add(&mut graph, &mut index, BlockNode::Proc(id));
            }
        }
    }

    let endpoints = collect_endpoints(design, module, &splits);

    // Sorted, because `HashMap` iteration order is randomised per process and
    // the edge order decides which routing track each wire gets. Left as-is,
    // the same design draws differently on every run.
    let mut nets: Vec<&NetId> = endpoints.keys().collect();
    nets.sort();

    let mut loops = Vec::new();
    for net in nets {
        let ends = &endpoints[net];
        for (driver, from_pin) in &ends.drivers {
            for (load, to_pin) in &ends.loads {
                if driver == load {
                    // Not an edge — see `Loop` — but not dropped either.
                    if index.contains_key(driver) {
                        loops.push(Loop {
                            node: *driver,
                            net: *net,
                            from_pin: from_pin.clone(),
                            to_pin: to_pin.clone(),
                            kind: clocks.kind_of(*net),
                        });
                    }
                    continue;
                }
                let (Some(from), Some(to)) = (index.get(driver), index.get(load)) else {
                    continue;
                };
                graph.add_edge(
                    *from,
                    *to,
                    BlockEdge {
                        net: *net,
                        from_pin: from_pin.clone(),
                        to_pin: to_pin.clone(),
                        kind: clocks.kind_of(*net),
                    },
                );
            }
        }
    }

    Block { graph, index, splits, loops }
}

#[derive(Default)]
struct Endpoints {
    /// `(box, pin name)` pairs that drive the net.
    drivers: Vec<(BlockNode, String)>,
    loads: Vec<(BlockNode, String)>,
}

/// The shared driver query, in the terms the diagram draws in.
///
/// Who drives a net is [`rtlscope_analyse::drive::endpoints`]'s answer and has
/// been since it stopped being written twice; all this does is name the ends
/// the way a picture needs them. A port is a box of its own here, and which box
/// depends on its direction, which is why the mapping is not quite the
/// identity.
fn collect_endpoints(
    design: &Design,
    module: &Module,
    splits: &HashMap<ProcId, Split>,
) -> HashMap<NetId, Endpoints> {
    let mut out: HashMap<NetId, Endpoints> = HashMap::new();
    for (net, ends) in rtlscope_analyse::drive::endpoints(design, module) {
        let entry = out.entry(net).or_default();
        for (from, into) in [(&ends.drivers, true), (&ends.loads, false)] {
            for end in from {
                // One end, usually. A cut process is the exception: the net it
                // *writes* comes out of the one stage that writes it, and the
                // net it *reads* goes into every stage that reads it — which
                // is what turns `d1` into a wire from one box to the next
                // rather than a pin on both sides of one.
                let nodes: Vec<BlockNode> = match end.at {
                    // An input drives the net inside the module and an output
                    // reads it, so which box an end belongs to is decided by
                    // the direction rather than by which list it came from.
                    drive::End::Port(port) => vec![match module.ports[port.0 as usize].dir {
                        PortDir::Input => BlockNode::InPort(port),
                        _ => BlockNode::OutPort(port),
                    }],
                    drive::End::Inst(inst, _) => vec![BlockNode::Inst(inst)],
                    drive::End::Proc(proc) => match splits.get(&proc) {
                        None => vec![BlockNode::Proc(proc)],
                        Some(split) if into => match split.stage_writing(net) {
                            Some(stage) => vec![BlockNode::Stage { proc, stage }],
                            // Written, but by no stage the analysis knows —
                            // which `split_of` rules out. Kept on the first
                            // stage rather than dropped on the floor.
                            None => split
                                .writes
                                .keys()
                                .next()
                                .map(|stage| BlockNode::Stage { proc, stage: *stage })
                                .into_iter()
                                .collect(),
                        },
                        Some(split) => split
                            .stages_reading(net)
                            .into_iter()
                            .map(|stage| BlockNode::Stage { proc, stage })
                            .collect(),
                    },
                };
                for node in nodes {
                    let pair = (node, end.pin.clone());
                    match into {
                        true => entry.drivers.push(pair),
                        false => entry.loads.push(pair),
                    }
                }
            }
        }
    }
    out
}

/// The nets in a module that carry a clock or a reset.
pub struct ClockNets {
    clocks: HashSet<NetId>,
    resets: HashSet<NetId>,
}

impl ClockNets {
    /// Finds them by looking at what the module's own flops are sensitive to,
    /// and at which child ports are clocks inside the child.
    pub fn of(design: &Design, module_id: ModuleId, clock_ports: &ClockPorts) -> Self {
        let module = design.module(module_id);
        let mut clocks = HashSet::new();
        let mut resets = HashSet::new();

        for process in &module.procs {
            if let ProcKind::Ff { clk, rst, .. } = &process.kind {
                extend(&mut clocks, clk);
                if let Some(reset) = rst {
                    extend(&mut resets, &reset.net);
                }
            }
        }

        // A net that feeds a child's clock port is a clock here too, which is
        // what keeps `clk` from being drawn across a whole hierarchy.
        for inst in &module.insts {
            let child_clocks = clock_ports.of_module(inst.of);
            for Conn { port, net: net_ref, .. } in &inst.conns {
                let Some(net) = net_ref.net_id() else { continue };
                match child_clocks.get(port) {
                    Some(WireKind::Clock) => {
                        clocks.insert(net);
                    }
                    Some(WireKind::Reset) => {
                        resets.insert(net);
                    }
                    _ => {}
                }
            }
        }

        Self { clocks, resets }
    }

    pub fn kind_of(&self, net: NetId) -> WireKind {
        if self.clocks.contains(&net) {
            WireKind::Clock
        } else if self.resets.contains(&net) {
            WireKind::Reset
        } else {
            WireKind::Signal
        }
    }
}

fn extend(set: &mut HashSet<NetId>, net_ref: &NetRef) {
    if let Some(net) = net_ref.net_id() {
        set.insert(net);
    }
}

/// Which ports of every module carry a clock or reset, seen from outside.
///
/// Computed once for the whole design and shared. Working it out per module on
/// demand looks harmless — each module asks its children — but the children ask
/// *their* children, and nothing is remembered, so the cost is exponential in
/// the depth of the hierarchy. On a real 35-module design that took longer than
/// ten minutes; memoised it is instant.
pub struct ClockPorts {
    by_module: HashMap<ModuleId, HashMap<PortId, WireKind>>,
}

impl ClockPorts {
    pub fn of(design: &Design) -> Self {
        let mut table = Self { by_module: HashMap::new() };
        for (id, _) in design.modules.iter_enumerated() {
            table.compute(design, id);
        }
        table
    }

    pub fn of_module(&self, module_id: ModuleId) -> &HashMap<PortId, WireKind> {
        static EMPTY: std::sync::OnceLock<HashMap<PortId, WireKind>> = std::sync::OnceLock::new();
        self.by_module.get(&module_id).unwrap_or_else(|| EMPTY.get_or_init(HashMap::new))
    }

    /// Depth-first with memoisation. The instance tree is acyclic — elaboration
    /// rejects anything else — so this terminates without a visited set.
    fn compute(&mut self, design: &Design, module_id: ModuleId) {
        if self.by_module.contains_key(&module_id) {
            return;
        }
        // Children first, so the lookup below always hits.
        for inst in &design.module(module_id).insts {
            self.compute(design, inst.of);
        }

        let module = design.module(module_id);
        let nets = ClockNets::of(design, module_id, self);
        let mut out = HashMap::new();
        for (position, port) in module.ports.iter().enumerate() {
            let kind = nets.kind_of(port.net);
            if kind != WireKind::Signal {
                out.insert(PortId(position as u32), kind);
            }
        }
        self.by_module.insert(module_id, out);
    }
}

/// Where a box came from in the source, for "jump to source".
pub fn span_of(design: &Design, module_id: ModuleId, node: BlockNode) -> Span {
    let module = design.module(module_id);
    match node {
        BlockNode::InPort(id) | BlockNode::OutPort(id) => module.port(id).span,
        BlockNode::Inst(id) => module.inst(id).span,
        BlockNode::Proc(id) | BlockNode::Stage { proc: id, .. } => module.proc(id).span,
    }
}
