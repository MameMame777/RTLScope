//! Reading a VCD or FST dump.
//!
//! Everything `wellen` is stops at this module. That is partly hygiene and
//! partly necessity: two of the types in its public signatures — `Bit` and
//! `DataOffset` — live in a private module and cannot be named from outside the
//! crate at all, so any struct field or function signature that tried to hold
//! one would not compile. Quarantining the dependency here means an upgrade
//! touches one file, and the rest of RTLScope sees [`WaveVar`] and [`WaveValue`].
//!
//! Two file-format differences shape the API:
//!
//! **FST is lazy, VCD is not.** Reading an FST header is cheap however large the
//! file, and [`Dump::load`] then pulls out only the signals asked for. A VCD is
//! parsed in full when it is opened. Both are hidden behind the same calls, but
//! it is why `load` exists rather than every signal simply being there.
//!
//! **A VCD must not be opened by path.** `wellen`'s path-based reader memory-maps
//! the file, and on Windows a live mapping holds it open — which blocks the
//! simulator that is still writing it, and is undefined behaviour if that
//! simulator truncates. Opening through a reader avoids the mapping entirely.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use wellen::viewers;
use wellen::{FileFormat, LoadOptions, Signal, SignalRef, SignalSource, TimeTableIdx, VarRef};

/// One variable in a dump, by the name it was recorded under.
///
/// Distinct from the signal behind it: a dump may record `clk`, `u_rx.clk` and
/// `u_tx.clk` as three variables sharing one signal, and it is the signal that
/// gets loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WaveVar(VarRef);

/// What a signal held at some moment.
///
/// A value containing `x` or `z` stays [`WaveValue::Unknown`] and never becomes
/// a number. Coercing it would be the one thing a decoder must not do: an
/// undriven bus that reads as zero is how a report ends up confidently wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaveValue {
    /// Every bit is 0 or 1, and there are at most 64 of them.
    Bits { value: u64, width: u32 },
    /// Every bit is 0 or 1, but there are more than 64.
    Wide { bits: String, width: u32 },
    /// At least one bit is `x` or `z`.
    Unknown { bits: String },
}

impl WaveValue {
    /// The value as a number, or `None` if any bit was not 0 or 1.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            WaveValue::Bits { value, .. } => Some(*value),
            WaveValue::Wide { .. } | WaveValue::Unknown { .. } => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            WaveValue::Bits { value, width: 1 } => Some(*value != 0),
            _ => None,
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, WaveValue::Unknown { .. })
    }

    pub fn width(&self) -> u32 {
        match self {
            WaveValue::Bits { width, .. } | WaveValue::Wide { width, .. } => *width,
            WaveValue::Unknown { bits } => bits.len() as u32,
        }
    }

    pub fn bit_string(&self) -> String {
        match self {
            WaveValue::Bits { value, width } => {
                (0..*width).rev().map(|bit| if value >> bit & 1 == 1 { '1' } else { '0' }).collect()
            }
            WaveValue::Wide { bits, .. } | WaveValue::Unknown { bits } => bits.clone(),
        }
    }

    fn from_bits(bits: &str) -> Self {
        let width = bits.len() as u32;
        if bits.bytes().any(|b| b != b'0' && b != b'1') {
            return WaveValue::Unknown { bits: bits.to_string() };
        }
        match (width <= 64).then(|| u64::from_str_radix(bits, 2).ok()).flatten() {
            Some(value) => WaveValue::Bits { value, width },
            None => WaveValue::Wide { bits: bits.to_string(), width },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WaveError {
    #[error("could not read `{path}`: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("`{path}` is not a waveform this can read: {reason}")]
    Parse { path: String, reason: String },
    /// Asked for the values of a signal that was never loaded — a bug in the
    /// caller rather than in the file, so it is named rather than papered over.
    #[error("`{0}` was not loaded; call `Dump::load` for it first")]
    NotLoaded(String),
    #[error("`{0}` is a real or string variable, which this does not model")]
    Unsupported(String),
}

/// A dump, with the signals that have been asked for loaded.
pub struct Dump {
    hierarchy: wellen::Hierarchy,
    source: Option<SignalSource>,
    time_table: Vec<u64>,
    format: FileFormat,
    /// Built once at open: `full_name` allocates a fresh string every call, and
    /// a design has thousands of variables.
    paths: Vec<(String, VarRef)>,
    loaded: std::collections::HashMap<SignalRef, Signal>,
}

impl Dump {
    /// Opens a dump, reading its structure but none of its values.
    pub fn open(path: &Path) -> Result<Self, WaveError> {
        let shown = path.display().to_string();
        let options = LoadOptions { multi_thread: true, remove_scopes_with_empty_name: false };

        let format = viewers::open_and_detect_file_format(path);
        let header = match format {
            // A VCD goes through a reader rather than the path, so that no
            // memory mapping is made of a file a simulator may still own.
            FileFormat::Vcd => {
                let file = File::open(path)
                    .map_err(|source| WaveError::Io { path: shown.clone(), source })?;
                viewers::read_header(BufReader::new(file), &options)
                    .map_err(|e| WaveError::Parse { path: shown.clone(), reason: e.to_string() })?
            }
            _ => viewers::read_header_from_file(path, &options)
                .map_err(|e| WaveError::Parse { path: shown.clone(), reason: e.to_string() })?,
        };

        let hierarchy = header.hierarchy;
        let body = viewers::read_body(header.body, &hierarchy, None)
            .map_err(|e| WaveError::Parse { path: shown, reason: e.to_string() })?;

        Ok(Self::assemble(hierarchy, Some(body.source), body.time_table, header.file_format))
    }

    /// Opens a VCD held in memory, which is what the decoder tests read.
    pub fn open_vcd_bytes(bytes: Vec<u8>) -> Result<Self, WaveError> {
        let options = LoadOptions { multi_thread: false, remove_scopes_with_empty_name: false };
        let reader = std::io::Cursor::new(bytes);
        let header = viewers::read_header(reader, &options)
            .map_err(|e| WaveError::Parse { path: "<memory>".into(), reason: e.to_string() })?;
        let hierarchy = header.hierarchy;
        let body = viewers::read_body(header.body, &hierarchy, None)
            .map_err(|e| WaveError::Parse { path: "<memory>".into(), reason: e.to_string() })?;
        Ok(Self::assemble(hierarchy, Some(body.source), body.time_table, FileFormat::Vcd))
    }

    fn assemble(
        hierarchy: wellen::Hierarchy,
        source: Option<SignalSource>,
        time_table: Vec<u64>,
        format: FileFormat,
    ) -> Self {
        let paths =
            hierarchy.all_vars().map(|var| (hierarchy[var].full_name(&hierarchy), var)).collect();
        Self { hierarchy, source, time_table, format, paths, loaded: Default::default() }
    }

    pub fn format(&self) -> FileFormat {
        self.format
    }

    /// The last moment the dump records, in raw ticks.
    pub fn max_time(&self) -> u64 {
        self.time_table.last().copied().unwrap_or(0)
    }

    pub fn time_table(&self) -> &[u64] {
        &self.time_table
    }

    /// How long one tick is, as (factor, unit) — `(1, "ns")` and so on.
    pub fn timescale(&self) -> Option<(u32, &'static str)> {
        self.hierarchy.timescale().map(|scale| (scale.factor, unit_name(scale.unit)))
    }

    /// A moment given in nanoseconds, as a tick of this dump.
    ///
    /// cocotb reports times in nanoseconds and a dump counts in whatever its
    /// timescale says, so something has to convert; doing it here means the
    /// dump's own timescale is what decides, rather than an assumption made
    /// somewhere else. `None` when the dump never declared one — better than
    /// a number that would be wrong by a factor of a thousand.
    pub fn ticks_of_ns(&self, ns: f64) -> Option<u64> {
        let scale = self.hierarchy.timescale()?;
        let per_ns = 1e-9 / (f64::from(scale.factor) * seconds_in(scale.unit)?);
        let ticks = ns * per_ns;
        if !ticks.is_finite() || ticks < 0.0 { None } else { Some(ticks.round() as u64) }
    }

    /// A tick of this dump, in nanoseconds.
    ///
    /// The inverse of [`Dump::ticks_of_ns`], for putting a number on a time
    /// axis: `1400` on its own means nothing, and a ruler that guessed the unit
    /// would be worse than one admitting it does not know. `None` when the dump
    /// declared no timescale — then the axis counts ticks and says so.
    pub fn ns_of_ticks(&self, ticks: u64) -> Option<f64> {
        let scale = self.hierarchy.timescale()?;
        let per_ns = 1e-9 / (f64::from(scale.factor) * seconds_in(scale.unit)?);
        if !per_ns.is_finite() || per_ns <= 0.0 {
            return None;
        }
        Some(ticks as f64 / per_ns)
    }

    /// Every variable, by its full dotted path.
    pub fn vars(&self) -> impl Iterator<Item = (&str, WaveVar)> + '_ {
        self.paths.iter().map(|(name, var)| (name.as_str(), WaveVar(*var)))
    }

    pub fn find(&self, full_path: &str) -> Option<WaveVar> {
        self.paths.iter().find(|(name, _)| name == full_path).map(|(_, var)| WaveVar(*var))
    }

    pub fn name_of(&self, var: WaveVar) -> String {
        self.hierarchy[var.0].full_name(&self.hierarchy)
    }

    /// How many bits a variable holds, or `None` for a real or string.
    pub fn width(&self, var: WaveVar) -> Option<u32> {
        self.hierarchy[var.0].length(&self.hierarchy)
    }

    /// Reads the values of these variables.
    ///
    /// One call per batch: aliases are collapsed and already-loaded signals
    /// dropped, so asking twice costs nothing, but each call re-reads whatever
    /// it is given.
    pub fn load(&mut self, vars: &[WaveVar]) -> Result<(), WaveError> {
        let mut wanted: Vec<SignalRef> = vars
            .iter()
            .map(|var| self.hierarchy[var.0].signal_ref())
            .filter(|signal| !self.loaded.contains_key(signal))
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        if wanted.is_empty() {
            return Ok(());
        }

        let Some(source) = self.source.as_mut() else {
            return Err(WaveError::NotLoaded("the dump has no signal source".into()));
        };
        for signal in source.load_signals(&wanted, &self.hierarchy, true) {
            self.loaded.insert(signal.signal_ref(), signal);
        }
        Ok(())
    }

    /// Every moment this variable changed, and what it changed to.
    pub fn changes(
        &self,
        var: WaveVar,
    ) -> Result<impl Iterator<Item = (u64, WaveValue)> + '_, WaveError> {
        let signal = self.signal(var)?;
        Ok(signal.iter_changes().map(|(idx, value)| {
            let time = self.time_table.get(idx as usize).copied().unwrap_or(0);
            (time, convert(&value))
        }))
    }

    /// What this variable held at a moment — the last change at or before it.
    pub fn value_at(&self, var: WaveVar, time: u64) -> Result<Option<WaveValue>, WaveError> {
        let signal = self.signal(var)?;
        if self.time_table.is_empty() {
            return Ok(None);
        }
        // The last time index that is not in the future.
        let at = self.time_table.partition_point(|&t| t <= time);
        if at == 0 {
            return Ok(None);
        }
        let idx = (at - 1) as TimeTableIdx;
        let Some(offset) = signal.get_offset(idx) else { return Ok(None) };
        // The last element settles the delta cycles at one timestamp.
        let value = signal.get_value_at(&offset, offset.elements - 1);
        Ok(Some(convert(&value)))
    }

    fn signal(&self, var: WaveVar) -> Result<&Signal, WaveError> {
        let signal_ref = self.hierarchy[var.0].signal_ref();
        self.loaded.get(&signal_ref).ok_or_else(|| WaveError::NotLoaded(self.name_of(var)))
    }
}

fn unit_name(unit: wellen::TimescaleUnit) -> &'static str {
    use wellen::TimescaleUnit::*;
    match unit {
        ZeptoSeconds => "zs",
        AttoSeconds => "as",
        FemtoSeconds => "fs",
        PicoSeconds => "ps",
        NanoSeconds => "ns",
        MicroSeconds => "us",
        MilliSeconds => "ms",
        Seconds => "s",
        Unknown => "?",
    }
}

/// How long one of these is in seconds, or `None` for a dump that did not say.
fn seconds_in(unit: wellen::TimescaleUnit) -> Option<f64> {
    use wellen::TimescaleUnit::*;
    Some(match unit {
        ZeptoSeconds => 1e-21,
        AttoSeconds => 1e-18,
        FemtoSeconds => 1e-15,
        PicoSeconds => 1e-12,
        NanoSeconds => 1e-9,
        MicroSeconds => 1e-6,
        MilliSeconds => 1e-3,
        Seconds => 1.0,
        Unknown => return None,
    })
}

fn convert(value: &wellen::SignalValueRef<'_>) -> WaveValue {
    match value.to_bit_string() {
        Some(bits) => WaveValue::from_bits(&bits),
        // A real, a string, or an event: none of them are bits, and pretending
        // otherwise is what `Unknown` exists to avoid.
        None => WaveValue::Unknown { bits: value.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_with_an_x_in_it_never_becomes_a_number() {
        let value = WaveValue::from_bits("10x1");
        assert!(value.is_unknown());
        assert_eq!(value.as_u64(), None);
        assert_eq!(value.bit_string(), "10x1");
    }

    #[test]
    fn a_value_too_wide_for_a_number_keeps_its_bits() {
        let bits = "1".repeat(65);
        let value = WaveValue::from_bits(&bits);
        assert_eq!(value.as_u64(), None);
        assert_eq!(value.width(), 65);
        assert_eq!(value.bit_string(), bits);
    }

    #[test]
    fn ordinary_values_read_as_numbers() {
        assert_eq!(WaveValue::from_bits("1010").as_u64(), Some(10));
        assert_eq!(WaveValue::from_bits("1").as_bool(), Some(true));
        assert_eq!(WaveValue::from_bits("0").as_bool(), Some(false));
        // A multi-bit value is not a bool, however it reads as a number.
        assert_eq!(WaveValue::from_bits("01").as_bool(), None);
    }

    /// A time axis has to put a number on itself, and the number has to be the
    /// one the reader would get back if they asked for that moment.
    #[test]
    fn ticks_and_nanoseconds_are_the_same_journey_either_way() {
        let vcd = b"$timescale 1ns $end
                    $scope module top $end
                    $var wire 1 ! a $end
                    $upscope $end
                    $enddefinitions $end
                    #0
0!
#40
1!
"
        .to_vec();
        let dump = Dump::open_vcd_bytes(vcd).expect("a dump");

        let ns = dump.ns_of_ticks(40).expect("it declared a timescale");
        assert!((ns - 40.0).abs() < 1e-6, "{ns}");
        assert_eq!(dump.ticks_of_ns(ns), Some(40));
    }
}
