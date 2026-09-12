//! The drawn form of a state machine.
//!
//! `rtlscope-analyse` reads a machine out of the IR — its states, what its next
//! value is a `case` on, which arm leads where — and can already print it. This
//! turns that into geometry, because a state machine is the one thing in a
//! design that nobody reads as a list. Its shape *is* the answer: whether the
//! reset state is reachable again, whether a state has a way out, whether the
//! machine is a ring or a fan.
//!
//! The layout is deliberately not the block diagram's. A block diagram flows
//! one way and can be ranked by a general algorithm; a state machine is a graph
//! with cycles by construction, and drawing it well means treating the three
//! kinds of transition differently:
//!
//! - **Forward**, into a later layer: a straight run through the channel.
//! - **Back**, to an earlier layer: down a lane, along under the whole diagram,
//!   and up another lane. Drawn straight it would cross every state between its
//!   ends.
//! - **Sideways**, between two states the same distance from reset: up or down
//!   a lane beside their own column.
//! - **A self-loop**: an arc over the top of its own box, where it cannot be
//!   confused with anything else.
//!
//! One rule keeps every one of those honest: a line may cross another line, but
//! never a state. Vertical runs happen only in the lanes between columns and
//! horizontal runs only in the channels between them or below everything, so no
//! route can be drawn through a box it does not join — which would read as
//! touching it. Arrivals are always on a state's left and departures on its
//! right or its own side lane, so which end of an arrow is which can be read
//! without following the line.
//!
//! Layers come from breadth-first distance from the reset state, which is the
//! order a reader follows: how many transitions from the start. Anything no
//! transition reaches — which the analysis reports and which is usually a bug —
//! is put in a layer of its own at the end rather than left where it would be
//! mistaken for part of the flow.

use std::collections::{BTreeMap, HashMap, VecDeque};

use rtlscope_analyse::{Fsm, fsm::ANY_OTHER};
use rtlscope_ir::Span;

use crate::geom::{Point, Rect};

/// Rough width of one character in the label font.
const CHAR_WIDTH: f64 = 7.5;
const BOX_HEIGHT: f64 = 34.0;
const MIN_BOX_WIDTH: f64 = 74.0;
/// Horizontal gap between layers, which is also the routing channel — wide
/// enough that a guard written on an arrow has somewhere to be.
const LAYER_GAP: f64 = 112.0;
/// Vertical gap between states in one layer.
const ROW_GAP: f64 = 26.0;
/// Distance between two routing tracks under the row. A track carries a
/// label as well as a line, so this is text height, not line width.
const TRACK_GAP: f64 = 15.0;
/// How far beside its own column a sideways edge runs, and how far apart two
/// of them are kept.
const LANE: f64 = 12.0;
const LANE_GAP: f64 = 7.0;
/// How high a self-loop rises above its box.
const LOOP_HEIGHT: f64 = 22.0;
const MARGIN: f64 = 24.0;
/// Wider than the rest, because the lanes that carry a transition back to the
/// reset state run down the left of the first column.
const LEFT_MARGIN: f64 = 56.0;
/// The leftmost a lane may be pushed before it would leave the page.
const LANE_MIN: f64 = 6.0;

/// One state, placed.
#[derive(Debug, Clone, PartialEq)]
pub struct StateNode {
    pub name: String,
    pub rect: Rect,
    /// Where the reset branch puts the machine.
    pub is_reset: bool,
    /// Where the `case`'s `default:` arm leads.
    ///
    /// Not an edge between two states: the arm starts from every encoding the
    /// machine does not name, and that is not a place to draw from. So it is
    /// a property of the state it arrives at, drawn the way the reset branch
    /// is — a stub coming in from outside the picture.
    pub from_any_other: bool,
    /// No transition leads here — dead code, or a way in this analysis cannot
    /// see. Either is worth drawing differently.
    pub unreachable: bool,
    /// Nothing leads out.
    pub terminal: bool,
    pub span: Span,
}

/// How a transition had to be drawn, which is also what it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeShape {
    /// Into a later layer: the machine making progress.
    Forward,
    /// Back to an earlier layer: the machine starting over.
    Back,
    /// Across to a state the same distance from reset.
    Sideways,
    /// A state that stays put under some condition.
    SelfLoop,
}

/// One transition, routed.
#[derive(Debug, Clone, PartialEq)]
pub struct StateEdge {
    /// Indices into [`StateGeom::states`].
    pub from: usize,
    pub to: usize,
    /// The guard, as the source wrote it. Empty when the arm takes this
    /// transition unconditionally.
    pub label: String,
    pub shape: EdgeShape,
    /// A polyline; the last point is the arrow head's tip.
    pub points: Vec<Point>,
    /// Where the label sits, when there is room for one.
    pub label_at: Point,
    pub span: Span,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateGeom {
    /// `module.state`, for a title.
    pub name: String,
    pub clock: String,
    pub width: f64,
    pub height: f64,
    pub states: Vec<StateNode>,
    pub edges: Vec<StateEdge>,
    /// Transitions the analysis found that this drawing has no place for —
    /// an arm leading to or from something the machine does not name as a
    /// state. Drawing them would mean inventing an endpoint; leaving them out
    /// silently would make the picture a lie about how many ways there are.
    pub dropped: Vec<String>,
}

impl StateGeom {
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// The state at a point, for hit testing.
    pub fn hit(&self, at: Point) -> Option<usize> {
        self.states.iter().position(|state| state.rect.contains(at))
    }
}

/// Lays a state machine out.
pub fn state_diagram(fsm: &Fsm) -> StateGeom {
    let mut geom = StateGeom {
        name: format!("{}.{}", fsm.module_name, fsm.state_name),
        clock: fsm.clock.clone(),
        ..StateGeom::default()
    };
    if fsm.states.is_empty() {
        return geom;
    }

    let index: HashMap<&str, usize> =
        fsm.states.iter().enumerate().map(|(at, state)| (state.name.as_str(), at)).collect();

    // Only transitions between states this machine actually has, because an
    // edge has to start and end somewhere.
    //
    // The catch-all arm is not one of the failures, though it fails the same
    // test. `default:` is how a careful machine handles the encodings it does
    // not name, so it is in nearly every machine worth reading — and counting
    // it as something the drawing could not manage put a warning badge on good
    // practice, which teaches the reader to stop looking at the badge. It has
    // no state to start from by construction, so it is carried to the state it
    // arrives at and drawn there instead. What is left over is a name this
    // drawing genuinely cannot place, and that is still worth saying.
    let mut links: Vec<Link> = Vec::new();
    let mut arrives_from_anywhere: Vec<usize> = Vec::new();
    for transition in &fsm.transitions {
        match (index.get(transition.from.as_str()), index.get(transition.to.as_str())) {
            (Some(from), Some(to)) => links.push(Link {
                from: *from,
                to: *to,
                guard: transition.guard.join(" && "),
                span: transition.span,
            }),
            (None, Some(to)) if transition.from == ANY_OTHER => arrives_from_anywhere.push(*to),
            _ => geom.dropped.push(format!("{} → {}", transition.from, transition.to)),
        }
    }

    let layers = layers(fsm, &index, &links);
    place(&mut geom, fsm, &layers);
    // After `place`, which is what builds the nodes to mark.
    for at in arrives_from_anywhere {
        geom.states[at].from_any_other = true;
    }
    route(&mut geom, &links);
    frame(&mut geom);
    geom
}

/// One transition between two states this machine actually declares.
struct Link {
    from: usize,
    to: usize,
    guard: String,
    span: Span,
}

/// Breadth-first distance from the reset state, with the unreachable last.
fn layers(fsm: &Fsm, index: &HashMap<&str, usize>, links: &[Link]) -> Vec<Vec<usize>> {
    let mut out: HashMap<usize, Vec<usize>> = HashMap::new();
    for link in links {
        out.entry(link.from).or_default().push(link.to);
    }

    let start = fsm
        .reset_state
        .as_deref()
        .and_then(|name| index.get(name).copied())
        // A machine with no reset branch still has to start being drawn
        // somewhere, and the first state declared is where a reader looks.
        .unwrap_or(0);

    let mut depth: BTreeMap<usize, usize> = BTreeMap::new();
    let mut queue = VecDeque::from([(start, 0usize)]);
    while let Some((state, at)) = queue.pop_front() {
        if depth.contains_key(&state) {
            continue;
        }
        depth.insert(state, at);
        for next in out.get(&state).into_iter().flatten() {
            if !depth.contains_key(next) {
                queue.push_back((*next, at + 1));
            }
        }
    }

    let deepest = depth.values().copied().max().unwrap_or(0);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); deepest + 1];
    let mut stranded = Vec::new();
    for state in 0..fsm.states.len() {
        match depth.get(&state) {
            Some(at) => layers[*at].push(state),
            None => stranded.push(state),
        }
    }
    if !stranded.is_empty() {
        layers.push(stranded);
    }
    layers.retain(|layer| !layer.is_empty());
    layers
}

fn place(geom: &mut StateGeom, fsm: &Fsm, layers: &[Vec<usize>]) {
    let mut rects: Vec<Rect> =
        vec![Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }; fsm.states.len()];

    // Self-loops arch over the top, so every row starts far enough down for
    // them to fit.
    let mut x = LEFT_MARGIN;
    let tallest = layers.iter().map(Vec::len).max().unwrap_or(1) as f64;
    let column_height = tallest * BOX_HEIGHT + (tallest - 1.0).max(0.0) * ROW_GAP;

    for layer in layers {
        let width = layer
            .iter()
            .map(|state| box_width(&fsm.states[*state].name))
            .fold(MIN_BOX_WIDTH, f64::max);
        let height = layer.len() as f64 * BOX_HEIGHT + (layer.len() as f64 - 1.0) * ROW_GAP;
        // Centred against the tallest layer, so a chain reads as a straight
        // line rather than as a staircase.
        let mut y = MARGIN + LOOP_HEIGHT + (column_height - height) / 2.0;
        for state in layer {
            rects[*state] = Rect { x, y, width, height: BOX_HEIGHT };
            y += BOX_HEIGHT + ROW_GAP;
        }
        x += width + LAYER_GAP;
    }

    geom.states = fsm
        .states
        .iter()
        .enumerate()
        .map(|(at, state)| StateNode {
            name: state.name.clone(),
            rect: rects[at],
            is_reset: fsm.reset_state.as_deref() == Some(state.name.as_str()),
            // Set by `state_diagram` once the arms have been sorted through.
            from_any_other: false,
            unreachable: fsm.unreachable.contains(&state.name),
            terminal: fsm.terminal.contains(&state.name),
            span: state.span,
        })
        .collect();
}

fn route(geom: &mut StateGeom, links: &[Link]) {
    // Every edge that needs a lane gets its own, so two of them never lie on
    // top of one another; the ones running under the diagram get their own
    // track as well.
    let mut track = 0usize;
    let mut lane = 0usize;
    let floor = geom.states.iter().map(|state| state.rect.bottom()).fold(0.0, f64::max);

    for link in links {
        let (source, target) = (geom.states[link.from].rect, geom.states[link.to].rect);
        let (shape, points, label_at) = if link.from == link.to {
            self_loop(source)
        } else if source.x < target.x {
            forward(source, target)
        } else if (source.x - target.x).abs() < 0.5 {
            let routed = sideways(source, target, offset(lane));
            lane += 1;
            routed
        } else {
            let routed = back(source, target, floor + TRACK_GAP * (track + 1) as f64, offset(lane));
            track += 1;
            lane += 1;
            routed
        };
        geom.edges.push(StateEdge {
            from: link.from,
            to: link.to,
            label: link.guard.clone(),
            shape,
            points,
            label_at,
            span: link.span,
        });
    }
}

/// Right edge to left edge, through the channel between two layers.
fn forward(source: Rect, target: Rect) -> (EdgeShape, Vec<Point>, Point) {
    let start = Point::new(source.right(), source.y + source.height / 2.0);
    let end = Point::new(target.x, target.y + target.height / 2.0);
    let middle = (start.x + end.x) / 2.0;
    let points = if (start.y - end.y).abs() < 0.5 {
        vec![start, end]
    } else {
        // A dog-leg through the middle of the channel: two turns, never a
        // diagonal across a state.
        vec![start, Point::new(middle, start.y), Point::new(middle, end.y), end]
    };
    (EdgeShape::Forward, points, Point::new(middle, (start.y + end.y) / 2.0 - 8.0))
}

/// How far into a lane the nth edge to need one goes.
fn offset(lane: usize) -> f64 {
    LANE + LANE_GAP * lane as f64
}

/// Out of the source, down its lane, back under the whole diagram, and up the
/// lane in front of the target.
///
/// Not down out of the source's own bottom edge: a state with another state
/// below it in the same column would have its neighbour drawn through.
fn back(source: Rect, target: Rect, depth: f64, offset: f64) -> (EdgeShape, Vec<Point>, Point) {
    // Both verticals sit in the gaps between columns, where no state is.
    let down = source.right() + offset;
    let up = (target.x - offset).max(LANE_MIN);
    let start = Point::new(source.right(), source.y + source.height / 2.0);
    let end = Point::new(target.x, target.y + target.height / 2.0);
    let points = vec![
        start,
        Point::new(down, start.y),
        Point::new(down, depth),
        Point::new(up, depth),
        Point::new(up, end.y),
        end,
    ];
    (EdgeShape::Back, points, Point::new((down + up) / 2.0, depth - 3.0))
}

/// Out of the left, along the lane beside the column, and back in on the left.
///
/// The left rather than the right because the right of every column is the
/// channel the forward edges run down, and a machine's progress should be the
/// easiest thing in the picture to follow.
fn sideways(source: Rect, target: Rect, offset: f64) -> (EdgeShape, Vec<Point>, Point) {
    let lane = (source.x - offset).max(LANE_MIN);
    let start = Point::new(source.x, source.y + source.height / 2.0);
    let end = Point::new(target.x, target.y + target.height / 2.0);
    let points = vec![start, Point::new(lane, start.y), Point::new(lane, end.y), end];
    (EdgeShape::Sideways, points, Point::new(lane, (start.y + end.y) / 2.0 - 3.0))
}

/// An arc over the top of the box.
fn self_loop(source: Rect) -> (EdgeShape, Vec<Point>, Point) {
    let left = Point::new(source.x + source.width * 0.3, source.y);
    let right = Point::new(source.x + source.width * 0.7, source.y);
    let top = source.y - LOOP_HEIGHT;
    let points =
        vec![left, Point::new(left.x, top), Point::new(right.x, top), Point::new(right.x, right.y)];
    (EdgeShape::SelfLoop, points, Point::new(source.x + source.width / 2.0, top - 6.0))
}

/// Sizes the canvas around everything drawn, edges included.
///
/// Back edges run below the states and self-loops above them, so a canvas
/// measured from the boxes alone clips exactly the edges that were hardest to
/// route.
fn frame(geom: &mut StateGeom) {
    let mut width: f64 = 0.0;
    let mut height: f64 = 0.0;
    // The lanes reach left of the first column, and a clipped arrow tail is
    // worse than an empty strip.
    for state in &geom.states {
        width = width.max(state.rect.right());
        height = height.max(state.rect.bottom());
    }
    for edge in &geom.edges {
        for point in &edge.points {
            width = width.max(point.x);
            height = height.max(point.y);
        }
        height = height.max(edge.label_at.y);
    }
    geom.width = width + MARGIN;
    geom.height = height + MARGIN;
}

fn box_width(name: &str) -> f64 {
    (name.chars().count() as f64 * CHAR_WIDTH + 22.0).max(MIN_BOX_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtlscope_analyse::{FsmState, Transition};
    use rtlscope_ir::{ModuleId, NetId};

    fn machine(states: &[&str], reset: Option<&str>, links: &[(&str, &str)]) -> Fsm {
        Fsm {
            module: ModuleId::from_raw(0),
            module_name: "m".into(),
            state: NetId::from_raw(0),
            state_name: "state".into(),
            next_name: None,
            clock: "clk".into(),
            reset_state: reset.map(str::to_string),
            states: states
                .iter()
                .enumerate()
                .map(|(value, name)| FsmState {
                    name: (*name).to_string(),
                    value: value as i64,
                    span: Span::UNKNOWN,
                })
                .collect(),
            transitions: links
                .iter()
                .map(|(from, to)| Transition {
                    from: (*from).to_string(),
                    to: (*to).to_string(),
                    guard: vec!["g".to_string()],
                    span: Span::UNKNOWN,
                })
                .collect(),
            unreachable: Vec::new(),
            terminal: Vec::new(),
            span: Span::UNKNOWN,
        }
    }

    fn at(geom: &StateGeom, name: &str) -> StateNode {
        geom.states.iter().find(|state| state.name == name).expect(name).clone()
    }

    /// The order a reader follows: how many transitions from the start.
    #[test]
    fn states_are_laid_out_by_their_distance_from_reset() {
        let geom = state_diagram(&machine(&["A", "B", "C"], Some("A"), &[("A", "B"), ("B", "C")]));

        assert!(at(&geom, "A").rect.x < at(&geom, "B").rect.x);
        assert!(at(&geom, "B").rect.x < at(&geom, "C").rect.x);
        assert!(geom.width > 0.0 && geom.height > 0.0);
    }

    /// No two states may overlap, whatever the shape of the machine.
    #[test]
    fn no_two_states_are_drawn_on_top_of_each_other() {
        let geom = state_diagram(&machine(
            &["IDLE", "A", "B", "C", "DONE"],
            Some("IDLE"),
            &[
                ("IDLE", "A"),
                ("IDLE", "B"),
                ("IDLE", "C"),
                ("A", "DONE"),
                ("B", "DONE"),
                ("C", "DONE"),
            ],
        ));

        for (i, one) in geom.states.iter().enumerate() {
            for other in &geom.states[i + 1..] {
                assert!(
                    !one.rect.overlaps(&other.rect),
                    "`{}` and `{}` overlap",
                    one.name,
                    other.name
                );
            }
        }
    }

    /// The three kinds of transition are told apart, because each has to be
    /// drawn somewhere different to stay readable.
    #[test]
    fn each_transition_is_shaped_by_where_it_goes() {
        let geom =
            state_diagram(&machine(&["A", "B"], Some("A"), &[("A", "B"), ("B", "A"), ("B", "B")]));

        let shapes: Vec<EdgeShape> = geom.edges.iter().map(|edge| edge.shape).collect();
        assert_eq!(shapes, [EdgeShape::Forward, EdgeShape::Back, EdgeShape::SelfLoop]);
    }

    /// The one route that cannot be drawn under the row: both ends are in the
    /// same column, so "down, across, up" is a line of no width drawn straight
    /// through whatever sits between them.
    #[test]
    fn a_transition_between_two_states_in_one_column_goes_around_them() {
        let geom = state_diagram(&machine(
            &["A", "B", "C"],
            Some("A"),
            &[("A", "B"), ("A", "C"), ("B", "C")],
        ));

        let edge = geom
            .edges
            .iter()
            .find(|edge| edge.shape == EdgeShape::Sideways)
            .expect("B and C share a layer");
        let (b, c) = (at(&geom, "B").rect, at(&geom, "C").rect);
        assert!((b.x - c.x).abs() < 0.5, "the premise: one column");
        assert!(
            edge.points.iter().all(|point| point.x <= b.x),
            "the lane is beside the column, not through it: {:?}",
            edge.points
        );
    }

    /// Whatever the route, a line may cross another line but never a state:
    /// an arrow drawn through a box reads as touching it.
    #[test]
    fn no_edge_is_drawn_through_a_state_it_does_not_join() {
        let geom = state_diagram(&machine(
            &["IDLE", "A", "B", "C", "DONE"],
            Some("IDLE"),
            &[
                ("IDLE", "A"),
                ("IDLE", "B"),
                ("IDLE", "C"),
                ("A", "B"),
                ("C", "A"),
                ("B", "DONE"),
                ("DONE", "IDLE"),
                // The case the first attempt got wrong: a state with two more
                // below it in its own column, giving up on the machine.
                ("A", "IDLE"),
                ("C", "C"),
            ],
        ));

        for edge in &geom.edges {
            for (at, state) in geom.states.iter().enumerate() {
                if at == edge.from || at == edge.to {
                    continue;
                }
                for pair in edge.points.windows(2) {
                    let (low, high) = (pair[0].x.min(pair[1].x), pair[0].x.max(pair[1].x));
                    let (top, bottom) = (pair[0].y.min(pair[1].y), pair[0].y.max(pair[1].y));
                    let segment = Rect { x: low, y: top, width: high - low, height: bottom - top };
                    assert!(
                        !segment.overlaps(&state.rect),
                        "`{}` → `{}` runs through `{}`: {:?}",
                        geom.states[edge.from].name,
                        geom.states[edge.to].name,
                        state.name,
                        edge.points
                    );
                }
            }
        }
    }

    /// A back edge that ran straight would cross every state between its ends,
    /// so it goes under the row — and the canvas has to include it.
    #[test]
    fn a_back_edge_is_routed_below_the_states_and_still_fits() {
        let geom = state_diagram(&machine(
            &["A", "B", "C"],
            Some("A"),
            &[("A", "B"), ("B", "C"), ("C", "A")],
        ));

        let floor = geom.states.iter().map(|s| s.rect.bottom()).fold(0.0, f64::max);
        let back =
            geom.edges.iter().find(|edge| edge.shape == EdgeShape::Back).expect("a back edge");
        assert!(
            back.points.iter().any(|point| point.y > floor),
            "the back edge stays among the states: {:?}",
            back.points
        );
        assert!(
            back.points.iter().all(|point| point.y <= geom.height),
            "the canvas clips it: height {} vs {:?}",
            geom.height,
            back.points
        );
    }

    /// A self-loop rises above its own box, which is only visible if the canvas
    /// starts far enough down.
    #[test]
    fn a_self_loop_fits_above_its_state() {
        let geom = state_diagram(&machine(&["A"], Some("A"), &[("A", "A")]));
        let loop_edge = &geom.edges[0];
        assert!(loop_edge.points.iter().all(|point| point.y >= 0.0), "{:?}", loop_edge.points);
        assert!(loop_edge.label_at.y >= 0.0);
    }

    /// The catch-all arm has no state to start from, so it is not an edge —
    /// but it is not a failure either, and saying so put a warning on the
    /// `default:` that nearly every careful machine has. It belongs to the
    /// state it arrives at.
    #[test]
    fn the_catch_all_arm_marks_the_state_it_arrives_at() {
        let mut fsm = machine(&["A", "B"], Some("A"), &[("A", "B")]);
        fsm.transitions.push(Transition {
            from: ANY_OTHER.into(),
            to: "A".into(),
            guard: Vec::new(),
            span: Span::UNKNOWN,
        });

        let geom = state_diagram(&fsm);
        assert_eq!(geom.edges.len(), 1, "no edge is invented: {:#?}", geom.edges);
        assert!(geom.dropped.is_empty(), "and nothing is reported: {:?}", geom.dropped);
        assert!(geom.states[0].from_any_other, "A is where the arm lands");
        assert!(!geom.states[1].from_any_other, "and B is not");
    }

    /// An arm naming an endpoint that is not a state and is not the catch-all
    /// is the case the report exists for: a typo, or a way in this analysis
    /// could not follow. Leaving it out silently would make the picture a lie
    /// about how many ways there are.
    #[test]
    fn an_arm_to_a_name_that_is_not_a_state_is_still_reported() {
        let mut fsm = machine(&["A", "B"], Some("A"), &[("A", "B")]);
        fsm.transitions.push(Transition {
            from: "B".into(),
            to: "S_TYPO".into(),
            guard: Vec::new(),
            span: Span::UNKNOWN,
        });

        let geom = state_diagram(&fsm);
        assert_eq!(geom.edges.len(), 1, "{:#?}", geom.edges);
        assert_eq!(geom.dropped, ["B → S_TYPO"], "and it says so");
        assert!(geom.states.iter().all(|state| !state.from_any_other), "nothing was marked");
    }

    #[test]
    fn a_machine_with_no_states_is_not_a_panic() {
        let geom = state_diagram(&machine(&[], None, &[]));
        assert!(geom.is_empty());
    }
}
