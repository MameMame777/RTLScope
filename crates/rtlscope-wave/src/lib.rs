//! Reading waveform dumps, and decoding the protocols recorded on them.
//!
//! A dump is the one thing RTLScope reads that is not source: it says what the
//! design *did*, where the IR says what it *is*. The two are joined by name —
//! [`matching`] resolves a dump's dotted paths against the design's flattened
//! signals — and everything after that is the same shape as the rest of the
//! tool: one representation, several views on it.
//!
//! A protocol decoder is such a view. `tvalid && tready` is a beat, and a
//! falling SDA while SCL is high is a START; neither is in the dump, but both
//! are derivable from it. What a decoder cannot derive it reports — an
//! unrecognised synchroniser, a VALID that dropped before READY — on the same
//! principle the front end follows: a report that quietly guessed is worse than
//! one that says so.

pub mod bind;
pub mod compare;
pub mod decode;
pub mod dump;
pub mod flow;
pub mod latency;
pub mod matching;
pub mod stages;

pub use bind::{Suggestion, suggest};
pub use compare::{Comparison, Divergence, compare};
pub use decode::{Annotation, Binding, DecodeReport, Decoder, Level, ResolvedBindings};
pub use dump::{Dump, WaveError, WaveValue, WaveVar};
pub use flow::{Token, TokenFlow, TokenStep};
pub use latency::{Bin, LatencyError, LatencyReport, cross_check, latency};
pub use matching::{MatchReport, Matched, match_signals};
pub use stages::{Basis, Cell, Cycles, Layout, StageError, StageRow, StageView, cycles, occupancy};
