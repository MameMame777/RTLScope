//! Graphs derived from the IR, and the geometry the diagrams are drawn from.
//!
//! Two pictures live here: the block diagram of a module, built from the IR
//! directly, and the state-transition diagram of a machine `rtlscope-analyse`
//! found. Both stop at geometry — where every box and every line goes — so the
//! SVG writer and the window paint the same answer and a layout bug is caught
//! by a test rather than by looking.

pub mod block;
pub mod geom;
pub mod layout;
pub mod route;
pub mod state;
pub mod svg;

pub use block::Stages;
pub use block::{Block, BlockEdge, BlockGraph, BlockNode, WireKind};
pub use geom::{BoxKind, DiagramGeom, NodeBox, Pin, Point, Rect, Side, Wire};
pub use route::{diagram, diagram_staged};
pub use state::{EdgeShape, StateEdge, StateGeom, StateNode, state_diagram};
pub use svg::{SvgOptions, render};
