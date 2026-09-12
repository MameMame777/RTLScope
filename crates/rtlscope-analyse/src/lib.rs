//! Reading structure back out of the elaborated IR.
//!
//! Nothing here parses or elaborates anything: both analyses are views on the
//! same [`Design`](rtlscope_ir::Design) the block diagram is drawn from, which is
//! the whole point of having one intermediate representation. A state machine
//! is a register whose next value is decided by a `case` on itself; a clock
//! domain crossing is a flop reading something a flop on another clock wrote.
//! Both are already in the IR — they just have not been looked for.
//!
//! Both analyses say what they could not work out rather than guessing, on the
//! same principle as the front end: a state diagram missing a transition, or a
//! crossing report missing a crossing, is worse than one that says so.

pub mod cdc;
pub mod cone;
pub mod depth;
pub mod drive;
pub mod flat;
pub mod fsm;
pub mod lint;
pub mod pipeline;

pub use cdc::{Crossing, CrossingKind, Domain, DomainReport};
pub use depth::{DepthError, DepthPath, DepthReport, DepthWarning, SignalGraph, signal_graph};
pub use drive::{Driver, Endpoints, Trace, endpoints, trace};
pub use fsm::{Fsm, FsmState, Transition};
pub use lint::{CombLoop, DeadNet, Latch, LintReport};
pub use pipeline::{DomainDepth, PipelineReport, Stage};
