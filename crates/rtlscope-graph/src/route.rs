//! Placing boxes and routing wires.
//!
//! A layered layout gives ranks and an order within each rank. Everything a
//! diagram actually needs on top of that is here: box sizes driven by how many
//! pins they carry, pins on the left and right edges, and wires that run only
//! horizontally and vertically.
//!
//! Vertical runs are the part worth care. Every wire crossing between two
//! columns needs its own vertical track in the channel between them, or two
//! wires that happen to share an x would draw as one line and read as a short.
//! Tracks are allocated greedily per channel, widest span first, which is
//! enough to keep them apart without a full channel-routing algorithm.

use std::collections::HashMap;

use petgraph::stable_graph::NodeIndex;
use petgraph::visit::{EdgeRef, IntoEdgeReferences};
use rtlscope_ir::{Design, ModuleId, PortDir};

use crate::block::{Block, BlockEdge, BlockNode, ClockPorts, Stages, span_of};
use crate::geom::{BoxKind, DiagramGeom, NodeBox, Pin, Point, Rect, Side, Wire};
use crate::layout::{LayeredLayout, LongestPath, Ranking, pin_ports_to_the_edges};

/// Vertical distance between two pins on the same edge.
const PIN_SPACING: f64 = 22.0;
/// Space above the first pin and below the last.
const BOX_PADDING: f64 = 16.0;
/// Rough width of one character in the label font.
const CHAR_WIDTH: f64 = 7.0;
/// Horizontal gap between columns, which is also the routing channel.
const COLUMN_GAP: f64 = 110.0;
/// Vertical gap between boxes in a column.
const ROW_GAP: f64 = 34.0;
/// Distance between two vertical wire tracks in one channel.
const TRACK_GAP: f64 = 9.0;
/// How far a wire runs straight out of a pin before it may turn.
const STUB: f64 = 14.0;
const MARGIN: f64 = 24.0;
const MIN_BOX_WIDTH: f64 = 96.0;
const MIN_BOX_HEIGHT: f64 = 40.0;

pub fn diagram(design: &Design, module_id: ModuleId) -> DiagramGeom {
    diagram_staged(design, module_id, &Stages::of(design))
}

/// The diagram, with the pipeline stages worked out once by the caller.
///
/// For a window drawing one module after another as the reader walks the
/// hierarchy: the stages come from flattening the whole design, and doing that
/// once per box would make every drill-down pay for the first one again.
pub fn diagram_staged(design: &Design, module_id: ModuleId, stages: &Stages) -> DiagramGeom {
    diagram_with(design, module_id, &LongestPath::new(), stages)
}

pub fn diagram_with(
    design: &Design,
    module_id: ModuleId,
    engine: &dyn LayeredLayout,
    stages: &Stages,
) -> DiagramGeom {
    let block = crate::block::build_with(design, module_id, &ClockPorts::of(design), stages);
    let mut ranking = engine.rank(&block.graph);
    pin_ports_to_the_edges(&mut ranking, &block.graph);

    let module = design.module(module_id);
    let mut plans = plan_boxes(design, module_id, &block);
    place(&mut plans, &ranking);

    let boxes: Vec<NodeBox> = ranking
        .layers
        .iter()
        .flatten()
        .filter_map(|node| plans.get(node).map(BoxPlan::finish))
        .collect();

    let wires = route(design, module_id, &block, &ranking, &plans);

    let mut geom =
        DiagramGeom { module: module.shown().into_owned(), width: 0.0, height: 0.0, boxes, wires };
    fit_canvas(&mut geom);
    geom
}

/// Sizes the canvas around everything drawn, wires included.
///
/// Feedback wires run below the boxes and long wires above them, so a canvas
/// measured from the boxes alone clips exactly the wires that were hardest to
/// route. Anything that landed at a negative coordinate is shifted back in.
fn fit_canvas(geom: &mut DiagramGeom) {
    fn extremes(geom: &DiagramGeom) -> Option<(f64, f64, f64, f64)> {
        let points = geom.wires.iter().flat_map(|wire| wire.points.iter().copied()).chain(
            geom.boxes.iter().flat_map(|node| {
                [
                    Point::new(node.rect.x, node.rect.y),
                    Point::new(node.rect.right(), node.rect.bottom()),
                ]
            }),
        );
        let mut bounds: Option<(f64, f64, f64, f64)> = None;
        for point in points {
            bounds = Some(match bounds {
                None => (point.x, point.y, point.x, point.y),
                Some((x0, y0, x1, y1)) => {
                    (x0.min(point.x), y0.min(point.y), x1.max(point.x), y1.max(point.y))
                }
            });
        }
        bounds
    }

    let Some((min_x, min_y, _, _)) = extremes(geom) else { return };

    let shift_x = (MARGIN - min_x).max(0.0);
    let shift_y = (MARGIN - min_y).max(0.0);
    if shift_x > 0.0 || shift_y > 0.0 {
        for node in &mut geom.boxes {
            node.rect.x += shift_x;
            node.rect.y += shift_y;
            for pin in &mut node.pins {
                pin.at.x += shift_x;
                pin.at.y += shift_y;
            }
        }
        for wire in &mut geom.wires {
            for point in &mut wire.points {
                point.x += shift_x;
                point.y += shift_y;
            }
        }
    }

    let Some((_, _, max_x, max_y)) = extremes(geom) else { return };
    geom.width = max_x + MARGIN;
    geom.height = max_y + MARGIN;
}

// ------------------------------------------------------------------ boxes ---

/// One pin before it has a position.
struct PinPlan {
    name: String,
    width: u32,
    /// The net in the module being drawn, which for an instance pin is the
    /// parent's net rather than the child's.
    net: Option<rtlscope_ir::NetId>,
    /// The child's port, for an instance pin. `None` for a port box, whose
    /// own `BlockNode` already carries the id.
    port: Option<rtlscope_ir::PortId>,
}

struct BoxPlan {
    node: BlockNode,
    kind: BoxKind,
    label: String,
    sublabel: Option<String>,
    /// Pins on the left edge, in the order they are drawn.
    inputs: Vec<PinPlan>,
    outputs: Vec<PinPlan>,
    rect: Rect,
    span: rtlscope_ir::Span,
    blackbox: bool,
    skipped: usize,
    comb: bool,
}

impl BoxPlan {
    fn finish(&self) -> NodeBox {
        let mut pins = Vec::new();
        for (index, plan) in self.inputs.iter().enumerate() {
            pins.push(Pin {
                name: plan.name.clone(),
                side: Side::Left,
                at: Point::new(self.rect.x, self.pin_y(index)),
                width: plan.width,
                net: plan.net,
                port: plan.port,
            });
        }
        for (index, plan) in self.outputs.iter().enumerate() {
            pins.push(Pin {
                name: plan.name.clone(),
                side: Side::Right,
                at: Point::new(self.rect.right(), self.pin_y(index)),
                width: plan.width,
                net: plan.net,
                port: plan.port,
            });
        }
        NodeBox {
            node: self.node,
            kind: self.kind,
            label: self.label.clone(),
            sublabel: self.sublabel.clone(),
            rect: self.rect,
            pins,
            span: self.span,
            blackbox: self.blackbox,
            skipped: self.skipped,
            comb: self.comb,
        }
    }

    fn pin_y(&self, index: usize) -> f64 {
        self.rect.y + BOX_PADDING + PIN_SPACING * (index as f64 + 0.5)
    }

    fn size(&self) -> (f64, f64) {
        let widest_pin = self
            .inputs
            .iter()
            .chain(&self.outputs)
            .map(|plan| plan.name.chars().count())
            .max()
            .unwrap_or(0);
        let label_chars =
            self.label.chars().count().max(self.sublabel.as_ref().map_or(0, |s| s.chars().count()));

        // Two columns of pin names plus a gap, or the label, whichever is wider.
        let width = (widest_pin as f64 * 2.0 * CHAR_WIDTH + 40.0)
            .max(label_chars as f64 * CHAR_WIDTH + 24.0)
            .max(MIN_BOX_WIDTH);
        let rows = self.inputs.len().max(self.outputs.len()) as f64;
        let height = (rows * PIN_SPACING + BOX_PADDING * 2.0).max(MIN_BOX_HEIGHT);
        (width, height)
    }
}

fn plan_boxes(design: &Design, module_id: ModuleId, block: &Block) -> HashMap<NodeIndex, BoxPlan> {
    let module = design.module(module_id);
    let mut plans = HashMap::new();

    for index in block.graph.node_indices() {
        let node = block.graph[index];
        let span = span_of(design, module_id, node);

        let mut plan = match node {
            BlockNode::InPort(id) => {
                let port = module.port(id);
                let net = module.net(port.net);
                BoxPlan {
                    node,
                    kind: BoxKind::InputPort,
                    label: port.shown().to_string(),
                    sublabel: None,
                    inputs: Vec::new(),
                    outputs: vec![PinPlan {
                        name: port.shown().to_string(),
                        width: net.width,
                        net: Some(port.net),
                        port: None,
                    }],
                    rect: Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
                    span,
                    blackbox: false,
                    skipped: 0,
                    comb: false,
                }
            }
            BlockNode::OutPort(id) => {
                let port = module.port(id);
                let net = module.net(port.net);
                BoxPlan {
                    node,
                    kind: BoxKind::OutputPort,
                    label: port.shown().to_string(),
                    sublabel: None,
                    inputs: vec![PinPlan {
                        name: port.shown().to_string(),
                        width: net.width,
                        net: Some(port.net),
                        port: None,
                    }],
                    outputs: Vec::new(),
                    rect: Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
                    span,
                    blackbox: false,
                    skipped: 0,
                    comb: false,
                }
            }
            BlockNode::Inst(id) => {
                let inst = module.inst(id);
                let child = design.module(inst.of);
                // Every port of the child is a pin, connected or not: a missing
                // wire on a drawn pin is information, an absent pin is not.
                let mut inputs = Vec::new();
                let mut outputs = Vec::new();
                for (position, port) in child.ports.iter().enumerate() {
                    let connected = inst
                        .conns
                        .iter()
                        .find(|conn| conn.port.0 as usize == position)
                        .and_then(|conn| conn.net.net_id());
                    let plan = PinPlan {
                        name: port.shown().to_string(),
                        width: child.net(port.net).width,
                        net: connected,
                        port: Some(rtlscope_ir::PortId(position as u32)),
                    };
                    match port.dir {
                        PortDir::Input => inputs.push(plan),
                        _ => outputs.push(plan),
                    }
                }
                BoxPlan {
                    node,
                    kind: BoxKind::Instance,
                    label: inst.shown().to_string(),
                    sublabel: Some(child.shown().into_owned()),
                    inputs,
                    outputs,
                    rect: Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
                    span,
                    blackbox: child.is_blackbox,
                    skipped: child.skipped.len(),
                    comb: false,
                }
            }
            BlockNode::Stage { proc, stage } => {
                let process = module.proc(proc);
                let split = &block.splits[&proc];
                let pin = |net: rtlscope_ir::NetId| PinPlan {
                    name: module.net(net).shown().to_string(),
                    width: module.net(net).width,
                    net: Some(net),
                    port: None,
                };
                // What this stage reads: the nets its own registers are
                // computed from, and whatever no register in particular reads —
                // the clock and the reset — which every stage takes.
                let inputs: Vec<PinPlan> = process
                    .reads
                    .iter()
                    .filter_map(rtlscope_ir::NetRef::net_id)
                    .filter(|net| split.stages_reading(*net).contains(&stage))
                    .map(pin)
                    .collect();
                let outputs: Vec<PinPlan> = process
                    .writes
                    .iter()
                    .filter_map(rtlscope_ir::NetRef::net_id)
                    .filter(|net| split.stage_writing(*net) == Some(stage))
                    .map(pin)
                    .collect();
                BoxPlan {
                    node,
                    kind: BoxKind::Process,
                    label: format!("stage {stage}"),
                    sublabel: Some(describe_process(&process.kind)),
                    inputs,
                    outputs,
                    rect: Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
                    span,
                    blackbox: false,
                    skipped: 0,
                    comb: false,
                }
            }
            BlockNode::Proc(id) => {
                let process = module.proc(id);
                let pins = |refs: &[rtlscope_ir::NetRef]| -> Vec<PinPlan> {
                    refs.iter()
                        .filter_map(|r| r.net_id())
                        .map(|net| PinPlan {
                            name: module.net(net).shown().to_string(),
                            width: module.net(net).width,
                            net: Some(net),
                            // A process reads and writes nets, not ports;
                            // there is no connection here to rewire.
                            port: None,
                        })
                        .collect()
                };
                BoxPlan {
                    node,
                    kind: BoxKind::Process,
                    label: describe_process(&process.kind),
                    sublabel: None,
                    inputs: pins(&process.reads),
                    outputs: pins(&process.writes),
                    rect: Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
                    span,
                    blackbox: false,
                    skipped: 0,
                    // A latch holds state too, in its own way, and an initial
                    // block is a value rather than logic; only `always_comb`
                    // and an `assign` are the cloud.
                    comb: matches!(process.kind, rtlscope_ir::ProcKind::Comb),
                }
            }
        };

        let (width, height) = plan.size();
        plan.rect.width = width;
        plan.rect.height = height;
        plans.insert(index, plan);
    }

    plans
}

fn describe_process(kind: &rtlscope_ir::ProcKind) -> String {
    match kind {
        rtlscope_ir::ProcKind::Comb => "comb".to_string(),
        rtlscope_ir::ProcKind::Latch => "latch".to_string(),
        rtlscope_ir::ProcKind::Initial => "initial".to_string(),
        rtlscope_ir::ProcKind::Ff { rst, .. } => {
            if rst.is_some() {
                "flop + reset".to_string()
            } else {
                "flop".to_string()
            }
        }
    }
}

/// Turns ranks and orders into coordinates.
fn place(plans: &mut HashMap<NodeIndex, BoxPlan>, ranking: &Ranking) {
    let mut x = MARGIN;
    for layer in &ranking.layers {
        let column_width = layer
            .iter()
            .filter_map(|node| plans.get(node))
            .map(|plan| plan.rect.width)
            .fold(0.0, f64::max);

        let mut y = MARGIN;
        for node in layer {
            let Some(plan) = plans.get_mut(node) else { continue };
            // Port boxes are flush against the side their pin is on, so every
            // wire leaving the input column starts at the same x and every wire
            // entering the output column ends at the same x. Anything else is
            // centred, which keeps a column looking like a column.
            let free = column_width - plan.rect.width;
            plan.rect.x = x + match plan.kind {
                BoxKind::InputPort => free,
                BoxKind::OutputPort => 0.0,
                _ => free / 2.0,
            };
            plan.rect.y = y;
            y += plan.rect.height + ROW_GAP;
        }
        x += column_width + COLUMN_GAP;
    }
}

// ------------------------------------------------------------------ wires ---

fn route(
    design: &Design,
    module_id: ModuleId,
    block: &Block,
    ranking: &Ranking,
    plans: &HashMap<NodeIndex, BoxPlan>,
) -> Vec<Wire> {
    let module = design.module(module_id);
    let layer_of: HashMap<NodeIndex, usize> = ranking
        .layers
        .iter()
        .enumerate()
        .flat_map(|(index, layer)| layer.iter().map(move |node| (*node, index)))
        .collect();

    // Everything below the lowest box, for feedback wires to run through.
    let floor = plans.values().map(|plan| plan.rect.bottom()).fold(0.0, f64::max) + ROW_GAP;

    // Where the boxes are in each column, so a wire crossing a column can be
    // steered into a gap instead of through a box.
    let columns: Vec<Vec<Rect>> = ranking
        .layers
        .iter()
        .map(|layer| layer.iter().filter_map(|node| plans.get(node)).map(|p| p.rect).collect())
        .collect();

    let mut channels: HashMap<usize, Vec<(f64, f64)>> = HashMap::new();
    let mut wires = Vec::new();

    // Longest spans first, so they take the outer tracks and short hops stay
    // close to the boxes they join.
    let mut edges: Vec<_> = (&block.graph).edge_references().collect();
    edges.sort_by_key(|edge: &petgraph::stable_graph::EdgeReference<'_, BlockEdge>| {
        let from = layer_of.get(&edge.source()).copied().unwrap_or(0) as i64;
        let to = layer_of.get(&edge.target()).copied().unwrap_or(0) as i64;
        -(to - from).abs()
    });

    for edge in edges {
        let (Some(source), Some(target)) = (plans.get(&edge.source()), plans.get(&edge.target()))
        else {
            continue;
        };
        let data = edge.weight();

        let Some(start) = pin_point(source, &data.from_pin, Side::Right) else { continue };
        let Some(end) = pin_point(target, &data.to_pin, Side::Left) else { continue };

        let from_layer = layer_of.get(&edge.source()).copied().unwrap_or(0);
        let to_layer = layer_of.get(&edge.target()).copied().unwrap_or(0);

        let points = if to_layer > from_layer {
            forward(start, end, from_layer, to_layer, &columns, &mut channels)
        } else {
            // A feedback path. Running it under the diagram keeps it out of the
            // forward channels, where it would otherwise cross everything.
            feedback(start, end, floor)
        };

        let net = module.net(data.net);
        wires.push(Wire {
            net: data.net,
            label: wire_label(net),
            kind: data.kind,
            points,
            span: net.span,
        });
    }

    // Last, and routed as the feedback they are: out of the pin, under the box,
    // and back into it. Under *that* box rather than the floor every other
    // feedback wire shares, because a loop is about one box and should hug it —
    // sent to the floor of a tall diagram it would read as a wire to somewhere
    // far away that happened to come back.
    for it in &block.loops {
        let Some(plan) = block.node(it.node).and_then(|node| plans.get(&node)) else { continue };
        let Some(start) = pin_point(plan, &it.from_pin, Side::Right) else { continue };
        let Some(end) = pin_point(plan, &it.to_pin, Side::Left) else { continue };
        let net = module.net(it.net);
        wires.push(Wire {
            net: it.net,
            label: wire_label(net),
            kind: it.kind,
            points: feedback(start, end, plan.rect.bottom() + ROW_GAP / 2.0),
            span: net.span,
        });
    }

    wires
}

/// What a wire is called: the net's name, with its width when it has one.
fn wire_label(net: &rtlscope_ir::Net) -> String {
    if net.width == 1 {
        net.shown().to_string()
    } else {
        format!("{}[{}:0]", net.name, net.width - 1)
    }
}

fn pin_point(plan: &BoxPlan, name: &str, side: Side) -> Option<Point> {
    let pins = match side {
        Side::Left => &plan.inputs,
        Side::Right => &plan.outputs,
    };
    let index = pins.iter().position(|plan| plan.name == name)?;
    let y = plan.pin_y(index);
    Some(match side {
        Side::Left => Point::new(plan.rect.x, y),
        Side::Right => Point::new(plan.rect.right(), y),
    })
}

/// Out of the source, through one channel per column crossed, into the target.
///
/// A wire that skips a column cannot simply run straight at the target's y: the
/// column in between has boxes at that height, and the wire would be drawn
/// through them. So it steps — a vertical hop in each channel, a horizontal run
/// across each column in a gap between that column's boxes.
///
/// Every bound comes from the *column*, never from the two boxes being joined.
/// A column holds boxes of different widths, and a channel placed just past the
/// source box can still be inside a wider neighbour sharing its column — which
/// is exactly how a wire came to be drawn through a box that had nothing to do
/// with it.
fn forward(
    start: Point,
    end: Point,
    from_layer: usize,
    to_layer: usize,
    columns: &[Vec<Rect>],
    channels: &mut HashMap<usize, Vec<(f64, f64)>>,
) -> Vec<Point> {
    // Adjacent columns with aligned pins: nothing is in the way.
    if to_layer == from_layer + 1 && (start.y - end.y).abs() < 0.5 {
        return vec![start, end];
    }

    let mut points = vec![start];
    let mut y = start.y;

    for channel in from_layer..to_layer {
        let left = column_right(columns, channel);
        let right = column_left(columns, channel + 1);

        // Where the wire should be by the time it reaches the far side: the
        // target's height for the last channel, a gap in the next column
        // otherwise.
        let target_y = if channel + 1 == to_layer {
            end.y
        } else {
            free_lane(columns.get(channel + 1).map(Vec::as_slice).unwrap_or(&[]), end.y)
        };

        if (y - target_y).abs() >= 0.5 {
            let span = (y.min(target_y), y.max(target_y));
            let x = channel_x(left, right, span, channel, channels);
            points.push(Point::new(x, y));
            points.push(Point::new(x, target_y));
        }
        y = target_y;
    }

    points.push(end);
    simplify(points)
}

/// The right edge of the widest box in a column.
fn column_right(columns: &[Vec<Rect>], index: usize) -> f64 {
    columns
        .get(index)
        .map(|column| column.iter().map(Rect::right).fold(f64::MIN, f64::max))
        .filter(|value| *value > f64::MIN)
        .unwrap_or(0.0)
}

/// The left edge of the leftmost box in a column.
fn column_left(columns: &[Vec<Rect>], index: usize) -> f64 {
    columns
        .get(index)
        .map(|column| column.iter().map(|rect| rect.x).fold(f64::MAX, f64::min))
        .filter(|value| *value < f64::MAX)
        .unwrap_or(0.0)
}

/// A vertical track in the gap between two columns.
fn channel_x(
    left: f64,
    right: f64,
    span: (f64, f64),
    channel: usize,
    channels: &mut HashMap<usize, Vec<(f64, f64)>>,
) -> f64 {
    let low = left + STUB;
    let high = right - STUB;
    let track = allocate_track(channels.entry(channel).or_default(), span);
    if high > low { (low + track as f64 * TRACK_GAP).min(high) } else { low }
}

/// A y a wire can cross this column at without touching a box.
///
/// Picks the gap — above the first box, between two, or below the last —
/// whose centre is nearest the height the wire is heading for.
fn free_lane(column: &[Rect], near: f64) -> f64 {
    if column.is_empty() {
        return near;
    }
    let mut spans: Vec<(f64, f64)> = column.iter().map(|r| (r.y, r.bottom())).collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut lanes = vec![spans[0].0 - ROW_GAP / 2.0];
    for pair in spans.windows(2) {
        lanes.push((pair[0].1 + pair[1].0) / 2.0);
    }
    lanes.push(spans.last().unwrap().1 + ROW_GAP / 2.0);

    lanes.into_iter().min_by(|a, b| (a - near).abs().total_cmp(&(b - near).abs())).unwrap_or(near)
}

/// Drops the zero-length segments the stepping can produce, so a wire has no
/// duplicate points and the orthogonality check stays meaningful.
fn simplify(points: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for point in points {
        if out.last().is_some_and(|last: &Point| {
            (last.x - point.x).abs() < 1e-9 && (last.y - point.y).abs() < 1e-9
        }) {
            continue;
        }
        out.push(point);
    }
    // Collapse a run of three collinear points into two.
    let mut i = 1;
    while i + 1 < out.len() {
        let (a, b, c) = (out[i - 1], out[i], out[i + 1]);
        let collinear = ((a.x - b.x).abs() < 1e-9 && (b.x - c.x).abs() < 1e-9)
            || ((a.y - b.y).abs() < 1e-9 && (b.y - c.y).abs() < 1e-9);
        if collinear {
            out.remove(i);
        } else {
            i += 1;
        }
    }
    out
}

/// Right, down below everything, back left, and up into the target.
fn feedback(start: Point, end: Point, floor: f64) -> Vec<Point> {
    let out = start.x + STUB * 2.0;
    let back = end.x - STUB * 2.0;
    vec![
        start,
        Point::new(out, start.y),
        Point::new(out, floor),
        Point::new(back, floor),
        Point::new(back, end.y),
        end,
    ]
}

/// The first track whose occupants do not overlap this wire's vertical span.
///
/// Two wires may share a track when their runs do not touch, which keeps the
/// channel narrow on wide diagrams.
fn allocate_track(occupied: &mut Vec<(f64, f64)>, span: (f64, f64)) -> usize {
    let mut track = 0;
    loop {
        let clash = occupied
            .iter()
            .enumerate()
            .any(|(index, other)| index == track && overlaps(*other, span));
        if !clash && track >= occupied.len() {
            occupied.push(span);
            return track;
        }
        if !clash {
            return track;
        }
        track += 1;
        if track > 200 {
            // A pathological fan-out; stacking further would look no worse.
            return track;
        }
    }
}

fn overlaps(a: (f64, f64), b: (f64, f64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// Which ports of the module are drawn on the outside, for the caller that
/// wants to label the diagram's own boundary.
pub fn boundary_ports(design: &Design, module_id: ModuleId) -> Vec<(String, PortDir)> {
    design.module(module_id).ports.iter().map(|port| (port.name.clone(), port.dir)).collect()
}
