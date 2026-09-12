//! Reading a protocol off a waveform.
//!
//! A decoder answers a question the dump does not: `tvalid && tready` is a
//! beat, a falling SDA while SCL is high is a START. None of that is
//! recorded — all of it is derivable — and deriving it is the difference
//! between looking at a hundred traces and reading what the design did.
//!
//! Every decoder is given [`Binding`]s from role to signal and hands back a
//! [`DecodeReport`]: annotations to draw, transactions to list, statistics to
//! read, and — the part that matters most — [`DecodeReport::problems`], where
//! everything it could not make sense of is named. A decoder that quietly
//! skipped a malformed packet would leave a report that looks complete and is
//! not, which is the one outcome worth more than any feature here.
//!
//! Two ways of walking a dump cover all four protocols. [`Sampler`] steps
//! through a clock's rising edges, which is what a synchronous bus is; and
//! [`edges`] merges two signals' changes in time order, which is what an
//! asynchronous one is.

pub mod axi4;
pub mod axis;
pub mod i2c;
pub mod pixel;

use serde::Serialize;

use crate::dump::{Dump, WaveError, WaveValue, WaveVar};

pub use axi4::Axi4;
pub use axis::AxiStream;
pub use i2c::I2c;
pub use pixel::PixelStream;

/// A signal a decoder needs, and what it is for.
#[derive(Debug, Clone, Copy)]
pub struct ChannelSpec {
    pub role: &'static str,
    pub required: bool,
    pub doc: &'static str,
}

/// A role, tied to a signal in the dump.
///
/// `invert` is for the open-drain idiom: a bus modelled as `scl_drive_low`
/// carries the opposite of the line, and a decoder reading it uninverted would
/// see every START as a STOP.
#[derive(Debug, Clone)]
pub struct Binding {
    pub role: String,
    pub path: String,
    pub invert: bool,
}

impl Binding {
    /// Parses `role=path`, or `role=!path` for a signal that reads inverted.
    pub fn parse(text: &str) -> Result<Self, DecodeError> {
        let Some((role, path)) = text.split_once('=') else {
            return Err(DecodeError::BadBinding(text.to_string()));
        };
        let (invert, path) = match path.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, path),
        };
        if role.is_empty() || path.is_empty() {
            return Err(DecodeError::BadBinding(text.to_string()));
        }
        Ok(Binding { role: role.to_string(), path: path.to_string(), invert })
    }
}

/// Bindings resolved against a dump, with the signals loaded.
#[derive(Debug)]
pub struct ResolvedBindings {
    channels: Vec<(String, WaveVar, bool)>,
}

impl ResolvedBindings {
    /// Looks each binding up, loads it, and checks that nothing required is
    /// missing.
    pub fn resolve(
        dump: &mut Dump,
        spec: &[ChannelSpec],
        bindings: &[Binding],
    ) -> Result<Self, DecodeError> {
        let mut channels = Vec::new();
        for binding in bindings {
            if !spec.iter().any(|s| s.role == binding.role) {
                let known: Vec<&str> = spec.iter().map(|s| s.role).collect();
                return Err(DecodeError::NoSuchRole {
                    role: binding.role.clone(),
                    known: known.join(", "),
                });
            }
            let Some(var) = dump.find(&binding.path) else {
                return Err(DecodeError::NoSuchSignal(binding.path.clone()));
            };
            channels.push((binding.role.clone(), var, binding.invert));
        }

        for required in spec.iter().filter(|s| s.required) {
            if !channels.iter().any(|(role, _, _)| role == required.role) {
                let bound: Vec<&str> = channels.iter().map(|(r, _, _)| r.as_str()).collect();
                return Err(DecodeError::MissingRole {
                    role: required.role,
                    doc: required.doc,
                    bound: bound.join(", "),
                });
            }
        }

        let vars: Vec<WaveVar> = channels.iter().map(|(_, var, _)| *var).collect();
        dump.load(&vars)?;
        Ok(ResolvedBindings { channels })
    }

    pub fn get(&self, role: &str) -> Option<(WaveVar, bool)> {
        self.channels.iter().find(|(r, _, _)| r == role).map(|(_, var, inv)| (*var, *inv))
    }

    pub fn has(&self, role: &str) -> bool {
        self.get(role).is_some()
    }

    /// What was bound to what, for the report.
    pub fn listing(&self, dump: &Dump) -> Vec<(String, String)> {
        self.channels
            .iter()
            .map(|(role, var, invert)| {
                let name = dump.name_of(*var);
                (role.clone(), if *invert { format!("!{name}") } else { name })
            })
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("`{0}` is not a binding; write it as `role=signal` or `role=!signal`")]
    BadBinding(String),
    #[error("`{role}` is not a channel of this protocol; it has {known}")]
    NoSuchRole { role: String, known: String },
    #[error("no signal named `{0}` in the dump")]
    NoSuchSignal(String),
    #[error("`{role}` must be bound ({doc}); only {bound} were given")]
    MissingRole { role: &'static str, doc: &'static str, bound: String },
    #[error(transparent)]
    Wave(#[from] WaveError),
}

/// How serious something a decoder found is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Warning,
    Error,
}

/// Something to draw on a lane beneath the signals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Annotation {
    pub t_start: u64,
    pub t_end: u64,
    /// Which lane it belongs on: 0 is the coarsest — frames, packets,
    /// transactions — and each row after that is finer.
    pub row: u8,
    pub kind: String,
    /// What to write in the box, if it fits.
    pub label: String,
    pub level: Level,
    /// The detail, for a tooltip or a `--json` reader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<(String, String)>,
}

impl Annotation {
    pub fn new(t_start: u64, t_end: u64, row: u8, kind: &str, label: String) -> Self {
        Self {
            t_start,
            t_end,
            row,
            kind: kind.to_string(),
            label,
            level: Level::Info,
            fields: Vec::new(),
        }
    }

    pub fn at(self, level: Level) -> Self {
        Self { level, ..self }
    }

    pub fn with(mut self, name: &str, value: impl std::fmt::Display) -> Self {
        self.fields.push((name.to_string(), value.to_string()));
        self
    }
}

/// One complete thing the protocol did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Transaction {
    pub t_start: u64,
    pub t_end: u64,
    pub kind: String,
    pub fields: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DecodeReport {
    pub protocol: String,
    pub bindings: Vec<(String, String)>,
    pub annotations: Vec<Annotation>,
    pub transactions: Vec<Transaction>,
    /// Counts and rates, in the order a reader wants them.
    pub stats: Vec<(String, String)>,
    /// Everything the decoder could not account for, in words. An empty list
    /// is the claim that nothing was skipped.
    pub problems: Vec<String>,
}

impl DecodeReport {
    pub fn new(protocol: &str) -> Self {
        Self { protocol: protocol.to_string(), ..Default::default() }
    }

    pub fn stat(&mut self, name: &str, value: impl std::fmt::Display) {
        self.stats.push((name.to_string(), value.to_string()));
    }

    pub fn problem(&mut self, text: impl Into<String>) {
        self.problems.push(text.into());
    }

    pub fn errors(&self) -> usize {
        self.annotations.iter().filter(|a| a.level == Level::Error).count()
    }
}

pub trait Decoder {
    fn protocol(&self) -> &'static str;
    fn doc(&self) -> &'static str;
    fn channels(&self) -> &'static [ChannelSpec];
    fn decode(&self, dump: &Dump, bindings: &ResolvedBindings) -> DecodeReport;
}

/// Every decoder there is.
pub fn all() -> Vec<Box<dyn Decoder>> {
    vec![Box::new(PixelStream), Box::new(AxiStream), Box::new(Axi4), Box::new(I2c)]
}

pub fn by_name(name: &str) -> Option<Box<dyn Decoder>> {
    all().into_iter().find(|decoder| decoder.protocol() == name)
}

// ----------------------------------------------------------- walking it ---

/// A signal's changes, held so they can be walked alongside others.
struct Track {
    changes: Vec<(u64, WaveValue)>,
    at: usize,
}

impl Track {
    /// The value at or before this moment, with the cursor left there.
    ///
    /// Times only ever go forwards, so the cursor only ever moves forwards:
    /// walking a whole dump costs one pass over each signal rather than a
    /// binary search per edge.
    fn advance_to(&mut self, time: u64) -> Option<&WaveValue> {
        while self.at + 1 < self.changes.len() && self.changes[self.at + 1].0 <= time {
            self.at += 1;
        }
        let (first, value) = self.changes.first()?;
        if time < *first {
            return None;
        }
        let _ = value;
        Some(&self.changes[self.at].1)
    }
}

fn invert(value: &WaveValue) -> WaveValue {
    match value {
        WaveValue::Bits { value, width } => {
            let mask = if *width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
            WaveValue::Bits { value: !value & mask, width: *width }
        }
        WaveValue::Wide { bits, width } => WaveValue::Wide {
            bits: bits.chars().map(|c| if c == '0' { '1' } else { '0' }).collect(),
            width: *width,
        },
        // Inverting something undriven leaves it undriven.
        WaveValue::Unknown { bits } => WaveValue::Unknown { bits: bits.clone() },
    }
}

/// Steps through a clock's rising edges, carrying the other signals along.
///
/// This is what a synchronous bus is: everything that matters happens at an
/// edge, and between edges nothing does.
pub struct Sampler {
    clock: Vec<u64>,
    tracks: Vec<(String, Track)>,
    at: usize,
}

/// What every bound signal held at one clock edge.
pub struct Sample<'a> {
    pub time: u64,
    tracks: &'a [(String, Track)],
}

impl Sample<'_> {
    pub fn value(&self, role: &str) -> Option<&WaveValue> {
        let (_, track) = self.tracks.iter().find(|(name, _)| name == role)?;
        Some(&track.changes[track.at].1)
    }

    /// A one-bit role read as a bool. `None` when unbound or undriven — the
    /// caller decides which, rather than being handed a `false`.
    pub fn high(&self, role: &str) -> Option<bool> {
        self.value(role)?.as_bool()
    }

    pub fn number(&self, role: &str) -> Option<u64> {
        self.value(role)?.as_u64()
    }
}

impl Sampler {
    /// Builds a sampler over the rising edges of `clock`.
    pub fn new(
        dump: &Dump,
        bindings: &ResolvedBindings,
        clock_role: &str,
        roles: &[&str],
    ) -> Result<Self, DecodeError> {
        let (clock_var, clock_invert) =
            bindings.get(clock_role).ok_or(DecodeError::MissingRole {
                role: "clock",
                doc: "the clock",
                bound: String::new(),
            })?;

        let mut edges = Vec::new();
        let mut was_high = false;
        for (time, value) in dump.changes(clock_var)? {
            let value = if clock_invert { invert(&value) } else { value };
            // An undriven clock is not an edge; it is the absence of one.
            let Some(high) = value.as_bool() else {
                was_high = false;
                continue;
            };
            if high && !was_high {
                edges.push(time);
            }
            was_high = high;
        }

        let mut tracks = Vec::new();
        for role in roles {
            let Some((var, invert_it)) = bindings.get(role) else { continue };
            let changes: Vec<(u64, WaveValue)> = dump
                .changes(var)?
                .map(|(t, v)| (t, if invert_it { invert(&v) } else { v }))
                .collect();
            tracks.push(((*role).to_string(), Track { changes, at: 0 }));
        }

        Ok(Sampler { clock: edges, tracks, at: 0 })
    }

    pub fn edges(&self) -> usize {
        self.clock.len()
    }

    /// The next rising edge, with every signal advanced to it.
    ///
    /// Not an `Iterator`: each `Sample` borrows the sampler it came from, so
    /// only one can be alive at a time.
    pub fn advance(&mut self) -> Option<Sample<'_>> {
        let time = *self.clock.get(self.at)?;
        self.at += 1;
        for (_, track) in &mut self.tracks {
            track.advance_to(time);
        }
        Some(Sample { time, tracks: &self.tracks })
    }
}

/// A moment, and what each of two signals held at it.
pub type Edge = (u64, Option<bool>, Option<bool>);

/// Two signals' changes in time order.
///
/// When both change at the same instant the first is delivered first. For I2C
/// that is what makes a STOP that releases SCL and SDA on one edge decode
/// correctly: the clock's rise is applied before the data's.
pub fn edges(
    dump: &Dump,
    bindings: &ResolvedBindings,
    first: &str,
    second: &str,
) -> Result<Vec<Edge>, DecodeError> {
    let read = |role: &str| -> Result<Vec<(u64, Option<bool>)>, DecodeError> {
        let Some((var, invert_it)) = bindings.get(role) else { return Ok(Vec::new()) };
        Ok(dump
            .changes(var)?
            .map(|(t, v)| {
                let v = if invert_it { invert(&v) } else { v };
                (t, v.as_bool())
            })
            .collect())
    };

    let a = read(first)?;
    let b = read(second)?;
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0usize, 0usize);
    let (mut a_now, mut b_now) = (None, None);

    while i < a.len() || j < b.len() {
        let next_a = a.get(i).map(|(t, _)| *t);
        let next_b = b.get(j).map(|(t, _)| *t);
        let time = match (next_a, next_b) {
            (Some(ta), Some(tb)) => ta.min(tb),
            (Some(ta), None) => ta,
            (None, Some(tb)) => tb,
            (None, None) => break,
        };
        // The first signal moves first at a shared instant.
        if next_a == Some(time) {
            a_now = a[i].1;
            i += 1;
        }
        if next_b == Some(time) {
            b_now = b[j].1;
            j += 1;
        }
        out.push((time, a_now, b_now));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binding_can_ask_for_the_opposite_of_a_signal() {
        let plain = Binding::parse("scl=tb.dut.scl").unwrap();
        assert_eq!(
            (plain.role.as_str(), plain.path.as_str(), plain.invert),
            ("scl", "tb.dut.scl", false)
        );

        let open_drain = Binding::parse("scl=!tb.dut.scl_drive_low").unwrap();
        assert!(open_drain.invert);
        assert_eq!(open_drain.path, "tb.dut.scl_drive_low");

        assert!(Binding::parse("nonsense").is_err());
        assert!(Binding::parse("=path").is_err());
        assert!(Binding::parse("role=").is_err());
    }

    #[test]
    fn inverting_something_undriven_leaves_it_undriven() {
        let unknown = WaveValue::Unknown { bits: "xx".into() };
        assert!(invert(&unknown).is_unknown());
        assert_eq!(
            invert(&WaveValue::Bits { value: 0b1010, width: 4 }),
            WaveValue::Bits { value: 0b0101, width: 4 }
        );
    }
}
