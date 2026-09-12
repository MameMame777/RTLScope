//! Layer assignment and within-layer ordering.
//!
//! `rust-sugiyama` does the hard part — ranking nodes and ordering each rank to
//! reduce crossings — and this module reads its answer back as a plain list of
//! layers. The pixels are placed by `route.rs`, because a block diagram needs
//! box sizes driven by pin counts and orthogonal wires, neither of which a
//! generic layered layout produces.
//!
//! Measured, not assumed: sugiyama ranks along **y** and spreads a rank along
//! **x**. A chain of four comes back with y = 0, 70, 140, 210 and a shared x.
//! Reading the axes the other way round would transpose every diagram, so the
//! ranks here are recovered by sorting the distinct y values rather than by
//! trusting a spacing formula.
//!
//! The trait exists so the engine can be swapped. Its quality on real designs
//! is the open question flagged in the plan, and the seam is where a
//! replacement would go.

use std::collections::{BTreeMap, HashMap, HashSet};

use petgraph::Direction;
use petgraph::stable_graph::NodeIndex;
use rust_sugiyama::configure::Config;

use crate::block::{BlockGraph, BlockNode};

/// Nodes grouped into layers, left to right in signal-flow order.
#[derive(Debug, Clone, Default)]
pub struct Ranking {
    pub layers: Vec<Vec<NodeIndex>>,
}

impl Ranking {
    pub fn layer_of(&self, node: NodeIndex) -> Option<usize> {
        self.layers.iter().position(|layer| layer.contains(&node))
    }

    pub fn is_empty(&self) -> bool {
        self.layers.iter().all(Vec::is_empty)
    }
}

pub trait LayeredLayout {
    fn rank(&self, graph: &BlockGraph) -> Ranking;
}

/// The default engine.
pub struct Sugiyama {
    pub config: Config,
}

impl Default for Sugiyama {
    fn default() -> Self {
        Self { config: Config { vertex_spacing: 40.0, ..Config::default() } }
    }
}

impl LayeredLayout for Sugiyama {
    fn rank(&self, graph: &BlockGraph) -> Ranking {
        if graph.node_count() == 0 {
            return Ranking::default();
        }

        let layouts = rust_sugiyama::from_graph(graph, &|_, _| (40.0, 20.0), &self.config);

        // Disjoint parts of a design get their own layout. Merging them by rank
        // rather than side by side keeps unrelated logic aligned in columns,
        // which is how a reader expects a block diagram to be organised.
        let mut by_rank: BTreeMap<i64, Vec<(f64, NodeIndex)>> = BTreeMap::new();
        let mut placed = Vec::new();

        for (positions, _width, _height) in &layouts {
            let mut ranks: Vec<i64> =
                positions.iter().map(|(_, (_, y))| y.round() as i64).collect();
            ranks.sort_unstable();
            ranks.dedup();

            for (node, (x, y)) in positions {
                let rank = ranks.binary_search(&(y.round() as i64)).unwrap_or(0);
                by_rank.entry(rank as i64).or_default().push((*x, *node));
                placed.push(*node);
            }
        }

        let mut layers: Vec<Vec<NodeIndex>> = by_rank
            .into_values()
            .map(|mut nodes| {
                // Within a rank, sugiyama's x is the crossing-minimised order.
                nodes.sort_by(|a, b| a.0.total_cmp(&b.0));
                nodes.into_iter().map(|(_, node)| node).collect()
            })
            .collect();

        // Anything the engine did not place — an isolated node, or a bug —
        // still has to be drawn, or the diagram would silently lose a box.
        let missing: Vec<NodeIndex> =
            graph.node_indices().filter(|node| !placed.contains(node)).collect();
        if !missing.is_empty() {
            layers.push(missing);
        }

        Ranking { layers }
    }
}

/// Moves the module's own ports to the outer columns.
///
/// A reader expects inputs on the left and outputs on the right. Sugiyama puts
/// them there already when the design is a clean pipeline, but any feedback
/// path can pull an output into the middle, and a diagram whose outputs are not
/// on the right is much harder to follow than one with a few longer wires.
pub fn pin_ports_to_the_edges(ranking: &mut Ranking, graph: &BlockGraph) {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();

    for layer in &mut ranking.layers {
        layer.retain(|node| match graph[*node] {
            BlockNode::InPort(_) => {
                inputs.push(*node);
                false
            }
            BlockNode::OutPort(_) => {
                outputs.push(*node);
                false
            }
            _ => true,
        });
    }
    ranking.layers.retain(|layer| !layer.is_empty());

    // Keep each port next to whatever it connects to, so the wires stay short.
    inputs.sort_by_key(|node| neighbour_order(graph, *node, Direction::Outgoing));
    outputs.sort_by_key(|node| neighbour_order(graph, *node, Direction::Incoming));

    if !inputs.is_empty() {
        ranking.layers.insert(0, inputs);
    }
    if !outputs.is_empty() {
        ranking.layers.push(outputs);
    }
}

/// A sort key that puts a port near the box it talks to.
fn neighbour_order(graph: &BlockGraph, node: NodeIndex, direction: Direction) -> usize {
    graph
        .neighbors_directed(node, direction)
        .map(|neighbour| neighbour.index())
        .min()
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockEdge, WireKind};
    use rtlscope_ir::{NetId, PortId};

    fn edge() -> BlockEdge {
        BlockEdge {
            net: NetId::from_raw(0),
            from_pin: "a".into(),
            to_pin: "b".into(),
            kind: WireKind::Signal,
        }
    }

    #[test]
    fn a_chain_becomes_one_node_per_layer() {
        let mut graph = BlockGraph::new();
        let a = graph.add_node(BlockNode::Inst(rtlscope_ir::InstId(0)));
        let b = graph.add_node(BlockNode::Inst(rtlscope_ir::InstId(1)));
        let c = graph.add_node(BlockNode::Inst(rtlscope_ir::InstId(2)));
        graph.add_edge(a, b, edge());
        graph.add_edge(b, c, edge());

        let ranking = Sugiyama::default().rank(&graph);
        assert_eq!(ranking.layers.len(), 3, "{:?}", ranking.layers);
        assert_eq!(ranking.layer_of(a), Some(0));
        assert_eq!(ranking.layer_of(c), Some(2));
    }

    #[test]
    fn every_node_lands_in_exactly_one_layer() {
        let mut graph = BlockGraph::new();
        let a = graph.add_node(BlockNode::Inst(rtlscope_ir::InstId(0)));
        let b = graph.add_node(BlockNode::Inst(rtlscope_ir::InstId(1)));
        graph.add_edge(a, b, edge());
        // An island with no edges at all still has to be drawn.
        let island = graph.add_node(BlockNode::Inst(rtlscope_ir::InstId(2)));

        let ranking = Sugiyama::default().rank(&graph);
        let total: usize = ranking.layers.iter().map(Vec::len).sum();
        assert_eq!(total, 3);
        assert!(ranking.layer_of(island).is_some(), "the island was dropped");
    }

    #[test]
    fn ports_are_moved_to_the_outer_columns() {
        let mut graph = BlockGraph::new();
        let input = graph.add_node(BlockNode::InPort(PortId(0)));
        let middle = graph.add_node(BlockNode::Inst(rtlscope_ir::InstId(0)));
        let output = graph.add_node(BlockNode::OutPort(PortId(1)));
        graph.add_edge(input, middle, edge());
        graph.add_edge(middle, output, edge());
        // A feedback path, which is what pulls an output out of the last column.
        graph.add_edge(output, middle, edge());

        let mut ranking = Sugiyama::default().rank(&graph);
        pin_ports_to_the_edges(&mut ranking, &graph);

        assert_eq!(ranking.layers.first().unwrap(), &[input]);
        assert_eq!(ranking.layers.last().unwrap(), &[output]);
    }
}

/// Layering without an external engine: break cycles, rank by longest path,
/// then order each layer by the barycentre of its neighbours.
///
/// This is the default. `rust-sugiyama` produces a better ordering on small
/// graphs, but it does not finish on real ones — measured on a 119-node,
/// 630-edge module from a real design, it had not returned after four
/// minutes, with or without its `transpose` pass. Every phase here is linear or
/// near-linear in the graph, and the same module ranks in milliseconds.
///
/// The trade is a slightly worse ordering, which shows up as a few more wire
/// crossings. A diagram that is drawn is worth more than one that is not.
#[derive(Debug, Default, Clone, Copy)]
pub struct LongestPath {
    /// Barycentre sweeps. Each pass costs one walk of the edges; four is well
    /// past the point where the ordering stops changing on real designs.
    pub sweeps: usize,
}

impl LongestPath {
    pub fn new() -> Self {
        Self { sweeps: 4 }
    }
}

impl LayeredLayout for LongestPath {
    fn rank(&self, graph: &BlockGraph) -> Ranking {
        if graph.node_count() == 0 {
            return Ranking::default();
        }

        let feedback = back_edges(graph);
        let ranks = longest_path_ranks(graph, &feedback);

        let depth = ranks.values().copied().max().unwrap_or(0);
        let mut layers: Vec<Vec<NodeIndex>> = vec![Vec::new(); depth + 1];
        // Sorted by index so the starting order — and therefore the finished
        // layout — does not depend on hash iteration.
        let mut nodes: Vec<NodeIndex> = graph.node_indices().collect();
        nodes.sort_by_key(|node| node.index());
        for node in nodes {
            layers[*ranks.get(&node).unwrap_or(&0)].push(node);
        }

        let sweeps = if self.sweeps == 0 { 4 } else { self.sweeps };
        order_by_barycentre(graph, &mut layers, sweeps);

        Ranking { layers }
    }
}

/// Edges that close a cycle, found by an iterative depth-first search.
///
/// RTL is full of feedback — a state register feeding the logic that computes
/// its own next value — so the block graph is never a DAG. Ranking needs one,
/// and these are the edges to leave out of it.
fn back_edges(graph: &BlockGraph) -> HashSet<(NodeIndex, NodeIndex)> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Open,
        Done,
    }

    let mut state: HashMap<NodeIndex, Mark> = HashMap::new();
    let mut feedback = HashSet::new();
    let mut roots: Vec<NodeIndex> = graph.node_indices().collect();
    roots.sort_by_key(|node| node.index());

    for root in roots {
        if state.contains_key(&root) {
            continue;
        }
        // Explicit stack: a deep hierarchy would overflow a recursive walk.
        let mut stack = vec![(root, graph.neighbors(root).collect::<Vec<_>>())];
        state.insert(root, Mark::Open);

        while let Some((node, pending)) = stack.last_mut() {
            let node = *node;
            match pending.pop() {
                Some(next) => match state.get(&next) {
                    // Still on the stack, so this edge closes a loop.
                    Some(Mark::Open) => {
                        feedback.insert((node, next));
                    }
                    Some(Mark::Done) => {}
                    None => {
                        state.insert(next, Mark::Open);
                        stack.push((next, graph.neighbors(next).collect()));
                    }
                },
                None => {
                    state.insert(node, Mark::Done);
                    stack.pop();
                }
            }
        }
    }
    feedback
}

/// Rank = the longest chain of forward edges reaching a node.
///
/// Repeats until nothing moves, which on a DAG is at most once per layer.
fn longest_path_ranks(
    graph: &BlockGraph,
    feedback: &HashSet<(NodeIndex, NodeIndex)>,
) -> HashMap<NodeIndex, usize> {
    let mut ranks: HashMap<NodeIndex, usize> = graph.node_indices().map(|node| (node, 0)).collect();

    // Bounded by the number of nodes: a longer chain than that is a cycle, and
    // the feedback edges are already excluded.
    for _ in 0..graph.node_count() {
        let mut changed = false;
        for edge in graph.edge_indices() {
            let Some((from, to)) = graph.edge_endpoints(edge) else { continue };
            if from == to || feedback.contains(&(from, to)) {
                continue;
            }
            let wanted = ranks[&from] + 1;
            if ranks[&to] < wanted {
                ranks.insert(to, wanted);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    ranks
}

/// Reduces crossings by moving each node next to the average position of its
/// neighbours in the layer before it, then the layer after, a few times over.
fn order_by_barycentre(graph: &BlockGraph, layers: &mut [Vec<NodeIndex>], sweeps: usize) {
    for pass in 0..sweeps {
        let forward = pass % 2 == 0;
        let indices: Vec<usize> = if forward {
            (1..layers.len()).collect()
        } else {
            (0..layers.len().saturating_sub(1)).rev().collect()
        };

        for index in indices {
            let neighbour_layer = if forward { index - 1 } else { index + 1 };
            let positions: HashMap<NodeIndex, usize> = layers[neighbour_layer]
                .iter()
                .enumerate()
                .map(|(position, node)| (*node, position))
                .collect();

            let direction =
                if forward { petgraph::Direction::Incoming } else { petgraph::Direction::Outgoing };

            let mut ordered: Vec<(f64, usize, NodeIndex)> = layers[index]
                .iter()
                .enumerate()
                .map(|(position, node)| {
                    let mut sum = 0.0;
                    let mut count = 0.0;
                    for neighbour in graph.neighbors_directed(*node, direction) {
                        if let Some(at) = positions.get(&neighbour) {
                            sum += *at as f64;
                            count += 1.0;
                        }
                    }
                    // A node with no neighbour in that layer keeps its place.
                    let key = if count > 0.0 { sum / count } else { position as f64 };
                    (key, position, *node)
                })
                .collect();

            // The original position breaks ties, so the sort is stable and the
            // layout is reproducible.
            ordered.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            layers[index] = ordered.into_iter().map(|(_, _, node)| node).collect();
        }
    }
}
