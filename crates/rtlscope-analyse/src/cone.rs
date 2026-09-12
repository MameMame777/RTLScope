//! What reaches a signal, and what it reaches.
//!
//! Two questions a reader asks constantly and a text search cannot answer:
//!
//! - *What decides `out_data`?* — everything upstream, through the mux, back
//!   through the registers to the logic and the memory that fed them.
//! - *What does `in_valid` disturb?* — everything downstream, through the state
//!   machine into the counter and out.
//!
//! Both are one walk over [`SignalGraph`], in opposite directions. The graph is
//! already the right one: [`crate::depth::signal_graph`] leaves clocks and
//! resets out of every fan-in set, because they decide *when* a value arrives
//! and not what it is. Without that a cone through any register would reach the
//! clock and from there the whole design, which is the classic way this
//! analysis becomes useless.
//!
//! Two more things keep it readable rather than complete. A level wider than
//! [`MAX_PER_LEVEL`] is cut, and a node reached across a register is marked so
//! the reader can see where a clock boundary was crossed. What is left out is
//! counted, never dropped silently — a cone that quietly stops is worse than no
//! cone, because it reads as an answer.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use crate::depth::SignalGraph;
use crate::flat::SignalId;

/// How a value gets from one signal to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// Across a register: it arrives on the next clock edge.
    Clocked,
    /// Through logic: it settles within the same clock.
    Comb,
}

/// Which way to walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Towards {
    /// What decides this signal.
    Drivers,
    /// What this signal decides.
    Loads,
}

/// One signal in the cone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConeNode {
    pub signal: SignalId,
    /// How many hops from the root. The root itself is zero.
    pub level: usize,
    /// Reached across a register, so its value is a clock behind the one
    /// before it. Marked rather than stopped at: where a pipeline's stages
    /// fall is exactly what a reader is often looking for.
    pub frontier: bool,
}

/// A signal, and what surrounds it in one direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cone {
    pub root: SignalId,
    pub towards: Towards,
    /// In breadth-first order, so a caller can draw levels as columns without
    /// sorting anything.
    pub nodes: Vec<ConeNode>,
    /// Always in the direction the value flows — `from` drives `to` — whichever
    /// way the walk went. A drawing should not have to know.
    pub edges: Vec<(SignalId, SignalId, EdgeKind)>,
    /// Signals a wide level left out. Reported so the picture can say so.
    pub clipped: usize,
}

impl Cone {
    /// How deep it actually goes, which is not always the depth asked for.
    pub fn depth(&self) -> usize {
        self.nodes.iter().map(|node| node.level).max().unwrap_or(0)
    }

    /// The signals at one distance from the root.
    pub fn level(&self, level: usize) -> impl Iterator<Item = &ConeNode> {
        self.nodes.iter().filter(move |node| node.level == level)
    }
}

/// How many signals one level may hold before the rest are left out.
///
/// A wide bus fans into a hundred bits and a reset reaches everything; drawing
/// all of it produces a picture with no shape, which answers nothing. Two dozen
/// is about as many boxes as a column can hold and still be read.
pub const MAX_PER_LEVEL: usize = 24;

/// What decides this signal, `depth` hops back.
pub fn fan_in(graph: &SignalGraph, root: SignalId, depth: usize) -> Cone {
    walk(graph, root, depth, Towards::Drivers)
}

/// What this signal decides, `depth` hops on.
pub fn fan_out(graph: &SignalGraph, root: SignalId, depth: usize) -> Cone {
    walk(graph, root, depth, Towards::Loads)
}

/// The one walk both questions are.
fn walk(graph: &SignalGraph, root: SignalId, depth: usize, towards: Towards) -> Cone {
    // Loads are the fan-in read backwards. Built once here rather than kept on
    // the graph, because the questions that need it are asked far less often
    // than the ones that do not.
    let loads = match towards {
        Towards::Loads => Some(transpose(graph)),
        Towards::Drivers => None,
    };
    let next = |of: SignalId| -> Vec<(SignalId, EdgeKind)> {
        match &loads {
            Some(loads) => loads.get(&of).cloned().unwrap_or_default(),
            None => {
                let clocked = graph.clocked.get(&of).into_iter().flatten();
                let comb = graph.combinational.get(&of).into_iter().flatten();
                clocked
                    .map(|signal| (*signal, EdgeKind::Clocked))
                    .chain(comb.map(|signal| (*signal, EdgeKind::Comb)))
                    .collect()
            }
        }
    };

    let mut nodes = vec![ConeNode { signal: root, level: 0, frontier: false }];
    let mut edges = Vec::new();
    let mut clipped = 0;
    let mut seen: HashSet<SignalId> = HashSet::from([root]);
    let mut queue: VecDeque<(SignalId, usize)> = VecDeque::from([(root, 0)]);

    while let Some((signal, level)) = queue.pop_front() {
        if level >= depth {
            continue;
        }
        // Sorted and deduplicated, so the same design draws the same picture
        // twice and a clip takes a stable half rather than an arbitrary one.
        let mut found: Vec<(SignalId, EdgeKind)> = next(signal);
        found.sort_by_key(|(signal, _)| *signal);
        found.dedup_by_key(|(signal, _)| *signal);

        let mut room = MAX_PER_LEVEL;
        for (other, kind) in found {
            if other == signal {
                // A signal in its own fan-in is a latch or a loop, which
                // `lint` and `depth` each have a finding for. Here it is noise.
                continue;
            }
            let (from, to) = match towards {
                Towards::Drivers => (other, signal),
                Towards::Loads => (signal, other),
            };
            if !seen.insert(other) {
                // Already in the cone: the edge is still worth drawing, since
                // reconvergence is a thing a reader wants to see.
                edges.push((from, to, kind));
                continue;
            }
            if room == 0 {
                clipped += 1;
                seen.remove(&other);
                continue;
            }
            room -= 1;
            edges.push((from, to, kind));
            nodes.push(ConeNode {
                signal: other,
                level: level + 1,
                frontier: kind == EdgeKind::Clocked,
            });
            queue.push_back((other, level + 1));
        }
    }

    Cone { root, towards, nodes, edges, clipped }
}

/// The fan-in read backwards: for each signal, what it decides.
fn transpose(graph: &SignalGraph) -> HashMap<SignalId, Vec<(SignalId, EdgeKind)>> {
    let mut loads: HashMap<SignalId, Vec<(SignalId, EdgeKind)>> = HashMap::new();
    let mut add = |sources: &BTreeSet<SignalId>, target: SignalId, kind: EdgeKind| {
        for source in sources {
            loads.entry(*source).or_default().push((target, kind));
        }
    };
    for (target, sources) in &graph.clocked {
        add(sources, *target, EdgeKind::Clocked);
    }
    for (target, sources) in &graph.combinational {
        add(sources, *target, EdgeKind::Comb);
    }
    loads
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sketch the feature was asked for, as a design:
    ///
    /// ```text
    ///   out_data          in_valid
    ///      ↑                 ↓
    ///     mux               fsm
    ///    ↗   ↖               ↓
    /// reg_a  reg_b        counter
    ///   ↑      ↑             ↓
    /// logic  memory      out_valid
    /// ```
    fn sketch() -> (rtlscope_ir::Design, crate::flat::Flattened) {
        // A directory of its own per call: these tests run in parallel and
        // shared one file, so a truncating write during another's read failed
        // only in the whole suite and never on its own.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let nth = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rtlscope-cone-{nth}"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("cone.sv");
        std::fs::write(
            &path,
            "module cone (\n\
                 input  logic       clk,\n\
                 input  logic       rst_n,\n\
                 input  logic       in_valid,\n\
                 input  logic [7:0] memory,\n\
                 input  logic       pick,\n\
                 output logic [7:0] out_data,\n\
                 output logic       out_valid\n\
             );\n\
                 logic [7:0] logic_in;\n\
                 logic [7:0] reg_a, reg_b;\n\
                 logic [1:0] fsm;\n\
                 logic [3:0] counter;\n\
                 assign logic_in = memory ^ 8'h5a;\n\
                 always_ff @(posedge clk) reg_a <= logic_in;\n\
                 always_ff @(posedge clk) reg_b <= memory;\n\
                 assign out_data = pick ? reg_a : reg_b;\n\
                 always_ff @(posedge clk) fsm <= in_valid ? 2'd1 : 2'd0;\n\
                 always_ff @(posedge clk) counter <= counter + {2'b0, fsm};\n\
                 assign out_valid = counter != 4'd0;\n\
             endmodule\n",
        )
        .expect("writes");

        let (uir, _) = rtlscope_sv::lower_files(&[path], &rtlscope_sv::ParseOptions::default());
        let design = rtlscope_elab::elaborate(&uir, Some("cone")).0.expect("elaborates");
        let flat = crate::flat::flatten(&design);
        (design, flat)
    }

    fn named(flat: &crate::flat::Flattened, design: &rtlscope_ir::Design, want: &str) -> SignalId {
        flat.all_names(design)
            .find(|(name, ..)| name == want || name.ends_with(&format!(".{want}")))
            .map(|(_, signal, ..)| signal)
            .unwrap_or_else(|| panic!("no signal called `{want}`"))
    }

    fn names(cone: &Cone, flat: &crate::flat::Flattened) -> std::collections::BTreeSet<String> {
        cone.nodes.iter().map(|node| flat.name_of(node.signal)).collect()
    }

    /// "What decides `out_data`?" — the mux's two registers, and behind them the
    /// logic and the memory that fed them. Three hops, which is why three is the
    /// default depth.
    #[test]
    fn a_mux_of_two_registers_traces_back_to_logic_and_memory() {
        let (design, flat) = sketch();
        let graph = crate::depth::signal_graph(&design, &flat);
        let cone = fan_in(&graph, named(&flat, &design, "out_data"), 3);

        let reached = names(&cone, &flat);
        for want in ["reg_a", "reg_b", "pick", "logic_in", "memory"] {
            assert!(
                reached.iter().any(|name| name.ends_with(want)),
                "{want} is upstream: {reached:?}"
            );
        }
        assert!(
            !reached.iter().any(|name| name.ends_with("out_valid")),
            "and this is not: {reached:?}"
        );
    }

    /// "What does `in_valid` disturb?" — the state machine, then the counter,
    /// then the output. The other sketch, read the other way.
    #[test]
    fn a_valid_reaches_the_output_through_the_fsm_and_counter() {
        let (design, flat) = sketch();
        let graph = crate::depth::signal_graph(&design, &flat);
        let cone = fan_out(&graph, named(&flat, &design, "in_valid"), 3);

        let reached = names(&cone, &flat);
        for want in ["fsm", "counter", "out_valid"] {
            assert!(
                reached.iter().any(|name| name.ends_with(want)),
                "{want} is downstream: {reached:?}"
            );
        }
        assert!(
            !reached.iter().any(|name| name.ends_with("reg_a")),
            "and this is not: {reached:?}"
        );

        // The levels are the pipeline: one register apiece.
        let at = |want: &str| {
            cone.nodes
                .iter()
                .find(|node| flat.name_of(node.signal).ends_with(want))
                .expect("in the cone")
        };
        assert_eq!(at("fsm").level, 1);
        assert_eq!(at("counter").level, 2);
        assert!(at("fsm").frontier, "reached across a register");
    }

    /// A clock reaches every register in its domain, and from there the whole
    /// design. A cone that followed one would answer every question with "all
    /// of it", which is the same as answering none.
    #[test]
    fn a_clock_is_not_an_influence() {
        let (design, flat) = sketch();
        let graph = crate::depth::signal_graph(&design, &flat);
        let cone = fan_in(&graph, named(&flat, &design, "out_data"), 6);

        let reached = names(&cone, &flat);
        assert!(!reached.iter().any(|name| name.ends_with("clk")), "no clock: {reached:?}");
        assert!(!reached.iter().any(|name| name.ends_with("rst_n")), "no reset: {reached:?}");
    }

    /// A wide level is cut, and the cut is counted. A picture that quietly
    /// stops is worse than none, because it reads as a complete answer.
    #[test]
    fn a_level_wider_than_it_can_draw_says_what_it_left_out() {
        let (design, flat) = sketch();
        let mut graph = crate::depth::signal_graph(&design, &flat);

        // A hundred sources on one target, which no drawing can hold. Well
        // clear of the real ids, so the root is not one of its own sources.
        let root = named(&flat, &design, "out_data");
        let crowd: BTreeSet<SignalId> = (1000..1100u32).collect();
        graph.combinational.insert(root, crowd);

        let cone = fan_in(&graph, root, 1);
        assert_eq!(cone.level(1).count(), MAX_PER_LEVEL, "as many as fit");
        assert_eq!(cone.clipped, 100 - MAX_PER_LEVEL, "and the rest are counted");
    }

    /// Read twice, drawn the same. A clip that took an arbitrary half would
    /// make a diagram that changed between two runs of one design.
    #[test]
    fn the_same_question_gives_the_same_cone_twice() {
        let (design, flat) = sketch();
        let graph = crate::depth::signal_graph(&design, &flat);
        let root = named(&flat, &design, "out_data");

        assert_eq!(fan_in(&graph, root, 3), fan_in(&graph, root, 3));
    }
}
