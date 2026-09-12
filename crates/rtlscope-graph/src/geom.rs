//! The drawn form of a block diagram.
//!
//! One geometry, two renderers. `svg.rs` writes a file and the GUI paints the
//! same structure with `egui::Painter`, so a layout bug shows up identically in
//! both and the tests can check the geometry rather than either output format.
//!
//! Coordinates are in abstract units with the origin at the top left, x running
//! left to right along the signal flow, and y down.

use rtlscope_ir::{NetId, PortId, Span};

use crate::block::{BlockNode, WireKind};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x
            && point.x <= self.right()
            && point.y >= self.y
            && point.y <= self.bottom()
    }

    /// True when two boxes overlap at all — the property a layout must not have.
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxKind {
    /// A port of the module being drawn.
    InputPort,
    OutputPort,
    Instance,
    /// An `always` block or a continuous assignment.
    Process,
}

#[derive(Debug, Clone)]
pub struct Pin {
    pub name: String,
    pub side: Side,
    pub at: Point,
    /// Bit width, shown on the wire as `[31:0]`.
    pub width: u32,
    /// Which port this pin is, for an instance box: the port of the *child*
    /// module, which is what an edit to the connection names.
    ///
    /// A name is not enough to edit by. Two ports can share one net, a name has
    /// to be looked up again to be used, and the pins of a box are inputs then
    /// outputs rather than declaration order — so the position in `pins` is not
    /// the port's id either. The router knows the id while it is building the
    /// pin and used to throw it away.
    pub port: Option<PortId>,
    /// The net this pin carries *in the module being drawn*, which for an
    /// instance is the parent's net, not the child's. `None` when the pin is
    /// unconnected. Highlighting a signal everywhere it goes needs this, and so
    /// will clicking a pin to add it to a waveform.
    pub net: Option<NetId>,
}

#[derive(Debug, Clone)]
pub struct NodeBox {
    /// Which graph node this is, for hit testing and drill-down.
    pub node: BlockNode,
    pub kind: BoxKind,
    pub label: String,
    /// The module a instance is of, or the kind of a process.
    pub sublabel: Option<String>,
    pub rect: Rect,
    pub pins: Vec<Pin>,
    /// Where in the source this box was written.
    pub span: Span,
    /// True when the instance is of a module RTLScope has no source for.
    pub blackbox: bool,
    /// How many constructs inside were skipped, so the box can be badged.
    pub skipped: usize,
    /// True for a process with no clock: logic that holds nothing between
    /// cycles. Drawn as a cloud, which is what a schematic has drawn stateless
    /// logic as for as long as there have been schematics, so that a reader
    /// scanning for where the clock stops can see it without reading a label.
    pub comb: bool,
}

/// The radius of the bumps a combinational box is drawn with, in sheet units.
///
/// Smaller boxes get smaller bumps — see [`cloud`] — so a cloud is never all
/// bump and no body.
pub const CLOUD_BUMP: f64 = 14.0;

/// How a combinational box is drawn: a ring of bumps around a body.
///
/// Both renderers draw from this, so the window and the SVG agree about where
/// every bump is, the same way they agree about every box and wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Cloud {
    /// Radius of every bump, in sheet units.
    pub radius: f64,
    /// The body the bumps sit on: the box's rect, pulled in by three quarters
    /// of a radius, so that each bump's crown clears the rect by a quarter and
    /// the dip between two bumps lands about on it — which is where the pins
    /// are, and a pin has to be on the outline.
    pub body: Rect,
    /// Centres of the bumps, on the body's edges, 1.4 radii apart: close enough
    /// that the ring is unbroken, far enough that each bump is still a bump.
    pub bumps: Vec<Point>,
}

pub fn cloud(rect: &Rect) -> Cloud {
    let radius = CLOUD_BUMP.min(rect.width * 0.4).min(rect.height * 0.4).max(1.0);
    let inset = radius * 0.75;
    let body = Rect {
        x: rect.x + inset,
        y: rect.y + inset,
        width: rect.width - 2.0 * inset,
        height: rect.height - 2.0 * inset,
    };
    let count = |length: f64| ((length / (radius * 1.4)).ceil() as usize).max(1) + 1;
    let columns = count(body.width);
    let rows = count(body.height);

    let mut bumps = Vec::new();
    for column in 0..columns {
        let x = body.x + body.width * column as f64 / (columns - 1) as f64;
        bumps.push(Point::new(x, body.y));
        bumps.push(Point::new(x, body.bottom()));
    }
    // The corners are already on the rows above, so the sides take the rest.
    for row in 1..rows - 1 {
        let y = body.y + body.height * row as f64 / (rows - 1) as f64;
        bumps.push(Point::new(body.x, y));
        bumps.push(Point::new(body.right(), y));
    }
    Cloud { radius, body, bumps }
}

#[derive(Debug, Clone)]
pub struct Wire {
    pub net: NetId,
    /// `alu_y[31:0]`, drawn near the middle of the run.
    pub label: String,
    pub kind: WireKind,
    /// An orthogonal polyline: every segment is horizontal or vertical.
    pub points: Vec<Point>,
    pub span: Span,
}

#[derive(Debug, Clone, Default)]
pub struct DiagramGeom {
    /// Name of the module drawn, for the title and the breadcrumb.
    pub module: String,
    pub width: f64,
    pub height: f64,
    pub boxes: Vec<NodeBox>,
    pub wires: Vec<Wire>,
}

impl DiagramGeom {
    /// The box under a point, innermost last so the topmost wins.
    pub fn hit(&self, point: Point) -> Option<&NodeBox> {
        self.boxes.iter().rev().find(|node| node.rect.contains(point))
    }

    /// Every wire carrying one net, for highlighting a signal everywhere it goes.
    pub fn wires_of(&self, net: NetId) -> impl Iterator<Item = &Wire> {
        self.wires.iter().filter(move |wire| wire.net == net)
    }

    /// The nets a box touches, for highlighting everything it is wired to.
    pub fn nets_of(&self, node: BlockNode) -> Vec<NetId> {
        let mut nets: Vec<NetId> = self
            .boxes
            .iter()
            .filter(|item| item.node == node)
            .flat_map(|item| item.pins.iter().filter_map(|pin| pin.net))
            .collect();
        nets.sort();
        nets.dedup();
        nets
    }

    /// Drops clock and reset wires, which is the default view: a diagram that
    /// draws them has a line from the clock to every flop and reads as noise.
    pub fn without_clocks(&self) -> Self {
        Self {
            wires: self
                .wires
                .iter()
                .filter(|wire| wire.kind == WireKind::Signal)
                .cloned()
                .collect(),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_is_symmetric_and_excludes_touching() {
        let a = Rect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 };
        let b = Rect { x: 5.0, y: 5.0, width: 10.0, height: 10.0 };
        let touching = Rect { x: 10.0, y: 0.0, width: 10.0, height: 10.0 };

        assert!(a.overlaps(&b) && b.overlaps(&a));
        assert!(!a.overlaps(&touching), "boxes that share an edge do not overlap");
    }

    #[test]
    fn contains_includes_the_border() {
        let rect = Rect { x: 1.0, y: 2.0, width: 4.0, height: 6.0 };
        assert!(rect.contains(Point::new(1.0, 2.0)));
        assert!(rect.contains(Point::new(5.0, 8.0)));
        assert!(!rect.contains(Point::new(5.1, 8.0)));
    }
}
