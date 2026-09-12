//! A waveform drawn by hand: what to drive, and what to expect back.
//!
//! This is the one thing in RTLScope that is *authored* rather than derived. The
//! design comes out of the source, the diagram out of the design, the waveform
//! out of a run — but a stimulus is a statement of intent, and somebody has to
//! make it. Drawing it is the shortest way to say "when `valid` goes up here,
//! `out_valid` should go up there", which is a testcase.
//!
//! # One truth, three shapes
//!
//! The pattern itself is the truth. Everything else is generated from it in the
//! same breath, so the shapes cannot drift apart:
//!
//! - **JSON** ([`Pattern::to_json`]) — what a cocotb test reads, with nothing
//!   but the Python standard library. This is the transport, and it is why the
//!   pattern stays *data*: redrawing and running again compiles nothing, where
//!   a harness with the values written into it would mean a Verilator rebuild
//!   for every cell repainted.
//! - **VCD** ([`Pattern::to_vcd`]) — for eyes. RTLScope already reads VCD, so a
//!   drawn pattern opens in the same waveform panel as the run it produced, and
//!   the two can be laid against each other.
//!
//! Plain VCD cannot say *which port a signal drives*, so the VCD carries a
//! convention of RTLScope's own: everything under the `rtlscope_stim` scope is
//! driven into the port of that name, everything under `rtlscope_expect` is
//! checked against it, and the `$comment` says which module and how long a
//! cycle is.
//!
//! # What a column means
//!
//! **Column `N` is the span between rising edge `N` and rising edge `N+1`.**
//! A drive lane's column `N` is what that input *holds* through the span, so
//! the design samples it at the edge that ends the span. An expect lane's
//! column `N` is what that output *held* through the same span. Both rows of
//! the drawing therefore mean the same thing about the same moment, which is
//! what makes a drawing readable — and it is the same reading as the waveform
//! panel, where a value occupies the space between two edges.
//!
//! Getting this wrong by one would put every check one cycle out, so the
//! generated test is written against this sentence and tested against a
//! pipeline deep enough for the answer to be unambiguous.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use rtlscope_ir::{Design, ModuleId, PortDir};

use crate::TbError;
use serde::{Deserialize, Serialize};

/// The widest port a drawn value can be given.
///
/// A cell is one number. Beyond this a number stops being one, and rounding
/// somebody's 128-bit bus down to its bottom half would be a lie about what
/// was driven — so wide ports are refused by name instead.
pub const MAX_WIDTH: u32 = 64;

/// What a lane holds for one column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Cell {
    /// A number, written in hexadecimal wherever this is text.
    Value(u64),
    /// On a drive lane: `x` is driven, which is how a testbench says "the
    /// design must not be looking at this". On an expect lane: not checked,
    /// which is the honest default — nobody knows every output every cycle.
    DontCare,
}

impl From<Cell> for String {
    fn from(cell: Cell) -> Self {
        match cell {
            Cell::Value(value) => format!("{value:x}"),
            Cell::DontCare => "x".to_string(),
        }
    }
}

impl TryFrom<String> for Cell {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let trimmed = text.trim();
        if trimmed.eq_ignore_ascii_case("x") || trimmed.eq_ignore_ascii_case("z") {
            return Ok(Cell::DontCare);
        }
        let digits = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        u64::from_str_radix(digits, 16)
            .map(Cell::Value)
            .map_err(|_| format!("`{text}` is not a hexadecimal value or `x`"))
    }
}

/// One port's row of the drawing.
///
/// Only the changes are kept, as a waveform is: between two of them the value
/// stands. Before the first change the lane holds [`Lane::initial`], and which
/// value that is depends on what the row is *for*:
///
/// - A row that **drives** starts at zero, because an input must be defined
///   from the first edge; nobody drawing a stimulus means "start this input
///   at `x`" by leaving it alone.
/// - A row that **expects** starts at [`Cell::DontCare`], because an undrawn
///   expectation is not an expectation of zero. Defaulting it the other way
///   would invent a check nobody wrote and then report the design for failing
///   it — which is the one thing this must never do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lane {
    pub port: String,
    pub width: u32,
    /// What the lane holds before its first change.
    pub initial: Cell,
    /// Ascending by cycle. Two entries for one cycle would make the drawing
    /// ambiguous, so [`Pattern::tidy`] removes them.
    pub changes: Vec<(u64, Cell)>,
}

impl Lane {
    /// A row that drives an input: defined from the start.
    pub fn driven(port: impl Into<String>, width: u32) -> Self {
        Self { port: port.into(), width, initial: Cell::Value(0), changes: Vec::new() }
    }

    /// A row that expects an output: checked only where it was drawn.
    pub fn expected(port: impl Into<String>, width: u32) -> Self {
        Self { port: port.into(), width, initial: Cell::DontCare, changes: Vec::new() }
    }

    /// What this lane holds during a column.
    pub fn at(&self, cycle: u64) -> Cell {
        match self.changes.partition_point(|(at, _)| *at <= cycle).checked_sub(1) {
            Some(index) => self.changes[index].1,
            None => self.initial,
        }
    }

    /// Draws a value into a column, leaving the rest of the lane alone.
    pub fn set(&mut self, cycle: u64, cell: Cell) {
        match self.changes.binary_search_by_key(&cycle, |(at, _)| *at) {
            Ok(index) => self.changes[index].1 = cell,
            Err(index) => self.changes.insert(index, (cycle, cell)),
        }
    }

    /// The bits this lane holds during a column, most significant first.
    fn bits(&self, cycle: u64) -> String {
        match self.at(cycle) {
            Cell::DontCare => "x".repeat(self.width as usize),
            Cell::Value(value) => (0..self.width)
                .rev()
                .map(|bit| if value >> bit & 1 == 1 { '1' } else { '0' })
                .collect(),
        }
    }
}

/// A drawing: a module, a clock to count by, and the lanes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
    /// The module this was drawn against, by its pre-specialisation name.
    pub module: String,
    /// The port whose rising edges the columns count.
    pub clock: String,
    pub period_ns: u32,
    /// How many columns the drawing is. Lanes may not reach this far; they
    /// hold their last value to the end, as a waveform does.
    pub cycles: u64,
    pub drive: Vec<Lane>,
    pub expect: Vec<Lane>,
}

impl Pattern {
    /// Reads a pattern from the text a GUI or a hand wrote.
    pub fn from_json(text: &str) -> Result<Pattern, String> {
        let mut pattern: Pattern =
            serde_json::from_str(text).map_err(|error| format!("{error}"))?;
        pattern.tidy();
        Ok(pattern)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// An empty drawing for a module: a row for every input that is neither
    /// clock nor reset, and a row for every output.
    ///
    /// Which port is a clock is the *design's* answer, already worked out from
    /// the `always_ff` blocks in the IR — see [`crate::clocks`]. Deciding it
    /// again here from port names would be a second opinion, and two opinions
    /// about which wire is the clock is one too many.
    pub fn blank(design: &Design, module: ModuleId, cycles: u64) -> Result<Pattern, TbError> {
        let plan = crate::clocks::plan(design, module);
        let module = &design.modules[module];
        let Some(clock) = plan.clocks.first() else {
            return Err(TbError::NoClock(module.base_name.clone()));
        };

        let width = |name: &str| {
            module
                .ports
                .iter()
                .find(|port| port.name == name)
                .map_or(1, |port| module.net(port.net).width)
        };
        Ok(Pattern {
            module: module.base_name.clone(),
            clock: clock.port.clone(),
            period_ns: clock.period_ns,
            cycles: cycles.max(1),
            drive: plan.tied_off.iter().map(|port| Lane::driven(port, width(port))).collect(),
            expect: module
                .ports
                .iter()
                .filter(|port| port.dir == PortDir::Output)
                .map(|port| Lane::expected(&port.name, module.net(port.net).width))
                .collect(),
        })
    }

    /// Brings a drawing up to date with a design that has been read again.
    ///
    /// Rows whose port has gone are removed and named; ports that have appeared
    /// get a blank row. A drawing that quietly kept driving a port the design
    /// no longer has would fail somewhere much further on, wearing a
    /// simulator's error rather than its own.
    pub fn reconcile(&mut self, fresh: &Pattern) -> Vec<String> {
        self.clock = fresh.clock.clone();
        self.period_ns = fresh.period_ns;

        let mut lost = Vec::new();
        for (mine, theirs, what) in
            [(&mut self.drive, &fresh.drive, "input"), (&mut self.expect, &fresh.expect, "output")]
        {
            mine.retain(|lane| {
                let kept = theirs.iter().any(|port| port.port == lane.port);
                if !kept {
                    lost.push(format!("the {what} `{}` is not in the design any more", lane.port));
                }
                kept
            });
            for lane in mine.iter_mut() {
                if let Some(port) = theirs.iter().find(|port| port.port == lane.port)
                    && port.width != lane.width
                {
                    lost.push(format!(
                        "`{}` is {} bits now, not {}; its row was cleared",
                        lane.port, port.width, lane.width
                    ));
                    lane.width = port.width;
                    lane.changes.clear();
                }
            }
            for port in theirs {
                if !mine.iter().any(|lane| lane.port == port.port) {
                    mine.push(port.clone());
                }
            }
        }
        lost
    }

    /// Puts the lanes in order and removes anything that would make a column
    /// mean two things, or say nothing at all.
    pub fn tidy(&mut self) {
        for lane in self.drive.iter_mut().chain(self.expect.iter_mut()) {
            lane.changes.sort_by_key(|(at, _)| *at);
            lane.changes.dedup_by_key(|(at, _)| *at);
            lane.changes.retain(|(at, _)| *at < self.cycles);

            // A change to the value already held is not a change. Painting a
            // run of cells with a drag makes one of these per column, and
            // leaving them in would put a `$var` transition in the VCD at every
            // one of them — a waveform of a signal that never moved.
            let mut held = lane.initial;
            lane.changes.retain(|(_, cell)| {
                let keep = *cell != held;
                held = *cell;
                keep
            });
        }
        self.cycles = self.cycles.max(1);
    }

    /// Ports too wide to draw a value into.
    ///
    /// Reported rather than silently narrowed: a harness that drove the bottom
    /// 64 bits of a 128-bit bus and said nothing would be describing a test
    /// nobody wrote.
    pub fn too_wide(&self) -> Vec<String> {
        self.drive
            .iter()
            .chain(self.expect.iter())
            .filter(|lane| lane.width > MAX_WIDTH)
            .map(|lane| {
                format!(
                    "`{}` is {} bits; a drawn value is at most {MAX_WIDTH}, so it is left \
                     tied off",
                    lane.port, lane.width
                )
            })
            .collect()
    }

    /// The lanes narrow enough to be driven, which is what the harnesses emit.
    pub fn drivable(&self) -> Vec<&Lane> {
        self.drive.iter().filter(|lane| lane.width <= MAX_WIDTH).collect()
    }

    pub fn checkable(&self) -> Vec<&Lane> {
        self.expect.iter().filter(|lane| lane.width <= MAX_WIDTH).collect()
    }

    /// The drawing as a waveform, so it can be looked at in the same panel as
    /// the run it produces.
    pub fn to_vcd(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "$version rtlscope $end");
        let _ = writeln!(
            out,
            "$comment rtlscope pattern for module `{}`, clock `{}`, {} ns per cycle, {} cycle(s). \
             Signals under `rtlscope_stim` are driven into the port of that name; those under \
             `rtlscope_expect` are checked against it. $end",
            self.module, self.clock, self.period_ns, self.cycles
        );
        let _ = writeln!(out, "$timescale 1ns $end");

        // An id per lane, from the printable range VCD uses.
        let mut ids: Vec<(String, &Lane)> = Vec::new();
        let mut next = 0usize;
        for (scope, lanes) in [("rtlscope_stim", &self.drive), ("rtlscope_expect", &self.expect)] {
            let _ = writeln!(out, "$scope module {scope} $end");
            for lane in lanes {
                let id = vcd_id(next);
                next += 1;
                let _ = writeln!(out, "$var wire {} {id} {} $end", lane.width, lane.port);
                ids.push((id, lane));
            }
            let _ = writeln!(out, "$upscope $end");
        }
        let _ = writeln!(out, "$enddefinitions $end");

        // Only the moments something changes, which is what a waveform is —
        // and the lanes change independently, so they are merged by column.
        let mut at: BTreeMap<u64, Vec<(String, String)>> = BTreeMap::new();
        for (id, lane) in &ids {
            for cycle in 0..self.cycles {
                if cycle == 0 || lane.changes.iter().any(|(when, _)| *when == cycle) {
                    at.entry(cycle).or_default().push((id.clone(), lane.bits(cycle)));
                }
            }
        }
        for (cycle, values) in at {
            let _ = writeln!(out, "#{}", cycle * u64::from(self.period_ns));
            for (id, bits) in values {
                if bits.len() == 1 {
                    let _ = writeln!(out, "{bits}{id}");
                } else {
                    let _ = writeln!(out, "b{bits} {id}");
                }
            }
        }
        out
    }
}

/// What a run of a drawn pattern came to.
///
/// Written by both harnesses in the same shape, and deliberately not the
/// cocotb `results.xml`: that file records how long a test ran, so a moment
/// taken from it is an accumulated estimate. Here the harness knows exactly
/// which column failed, and says so.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    #[serde(default)]
    pub checked: u64,
    #[serde(default)]
    pub failures: Vec<Mismatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mismatch {
    pub cycle: u64,
    #[serde(default)]
    pub time_ns: f64,
    pub port: String,
    pub expected: String,
    pub got: String,
}

impl Verdict {
    pub fn read(path: &Path) -> Result<Verdict, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|error| format!("{} is not a verdict: {error}", path.display()))
    }

    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// One line for a status bar.
    pub fn summary(&self) -> String {
        match self.failures.len() {
            0 => format!("{} check(s), all held", self.checked),
            1 => format!("{} check(s), 1 failed", self.checked),
            n => format!("{} check(s), {n} failed", self.checked),
        }
    }
}

/// The nth identifier VCD gives a signal.
fn vcd_id(index: usize) -> String {
    const FIRST: u8 = b'!';
    const SPAN: usize = (b'~' - b'!' + 1) as usize;
    let mut out = String::new();
    let mut left = index;
    loop {
        out.push(char::from(FIRST + (left % SPAN) as u8));
        left /= SPAN;
        if left == 0 {
            break;
        }
        left -= 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipeline3() -> Pattern {
        let mut valid = Lane::driven("in_valid", 1);
        valid.set(2, Cell::Value(1));
        valid.set(3, Cell::Value(0));
        let mut data = Lane::driven("in_data", 16);
        data.set(2, Cell::Value(0xbeef));
        data.set(3, Cell::DontCare);

        let mut out_valid = Lane::expected("out_valid", 1);
        out_valid.set(5, Cell::Value(1));
        out_valid.set(6, Cell::Value(0));

        Pattern {
            module: "pipeline3".into(),
            clock: "clk".into(),
            period_ns: 10,
            cycles: 8,
            drive: vec![valid, data],
            expect: vec![out_valid],
        }
    }

    /// A waveform holds its value between changes; so does a lane.
    #[test]
    fn a_lane_holds_between_the_columns_it_was_drawn_in() {
        let pattern = pipeline3();
        let valid = &pattern.drive[0];

        assert_eq!(valid.at(0), Cell::Value(0), "undrawn is defined, not unknown");
        assert_eq!(valid.at(1), Cell::Value(0));
        assert_eq!(valid.at(2), Cell::Value(1));
        assert_eq!(valid.at(3), Cell::Value(0));
        assert_eq!(valid.at(7), Cell::Value(0), "and holds to the end");
    }

    fn design(fixture: &str, top: Option<&str>) -> rtlscope_ir::Design {
        let path = rtlscope_fixtures::path(fixture);
        let (uir, _) = rtlscope_sv::lower_files(&[path], &rtlscope_sv::ParseOptions::default());
        rtlscope_elab::elaborate(&uir, top).0.expect("elaborates")
    }

    /// The rows come from the design's own answer about which port is which,
    /// not from a second guess made here.
    #[test]
    fn a_blank_drawing_has_a_row_per_port_that_is_not_a_clock() {
        let design = design("pipeline3.sv", Some("pipeline3"));
        let blank = Pattern::blank(&design, design.top, 16).expect("pipeline3 has a clock");

        assert_eq!(blank.clock, "clk");
        let driven: Vec<&str> = blank.drive.iter().map(|lane| lane.port.as_str()).collect();
        assert_eq!(driven, ["in_valid", "in_data"], "clk and rst_n are not drawn");
        let expected: Vec<&str> = blank.expect.iter().map(|lane| lane.port.as_str()).collect();
        assert_eq!(expected, ["out_valid", "out_data"]);
        assert_eq!(blank.drive[1].width, 16, "and the widths come from the design");

        // Nothing is claimed until something is drawn.
        assert_eq!(blank.expect[0].at(0), Cell::DontCare);
        assert_eq!(blank.drive[0].at(0), Cell::Value(0));
    }

    /// A drawing outlives the design being read again, so it has to be told
    /// when what it was drawn against has moved.
    #[test]
    fn reading_the_design_again_says_which_rows_no_longer_have_a_port() {
        let design = design("pipeline3.sv", Some("pipeline3"));
        let fresh = Pattern::blank(&design, design.top, 16).expect("a clock");

        let mut mine = fresh.clone();
        mine.drive.push(Lane::driven("gone_away", 1));
        mine.expect.remove(1);
        mine.drive[0].set(2, Cell::Value(1));

        let lost = mine.reconcile(&fresh);

        assert_eq!(lost.len(), 1, "{lost:?}");
        assert!(lost[0].contains("gone_away"), "{lost:?}");
        assert!(
            mine.expect.iter().any(|lane| lane.port == "out_data"),
            "a port that appeared gets a row"
        );
        assert_eq!(mine.drive[0].changes, [(2, Cell::Value(1))], "and what was drawn survives");
    }

    /// Painting a run of cells with a drag writes the same value into each of
    /// them; keeping those would put a transition in the waveform at every
    /// column of a signal that never moved.
    #[test]
    fn a_value_repainted_over_itself_is_not_a_change() {
        let mut lane = Lane::driven("in_valid", 1);
        for column in 2..6 {
            lane.set(column, Cell::Value(1));
        }
        let mut pattern = pipeline3();
        pattern.drive[0] = lane;

        pattern.tidy();

        assert_eq!(pattern.drive[0].changes, [(2, Cell::Value(1))], "one edge, not four");
        assert_eq!(pattern.drive[0].at(5), Cell::Value(1), "and it still holds through them");
    }

    #[test]
    fn the_pattern_survives_the_round_trip_it_travels_to_cocotb_by() {
        let pattern = pipeline3();
        let back = Pattern::from_json(&pattern.to_json()).expect("reads");
        assert_eq!(back, pattern);
    }

    /// `x` has to survive as `x`. A don't-care that came back as zero would be
    /// a check nobody wrote, passing.
    #[test]
    fn a_dont_care_stays_a_dont_care_through_text() {
        assert_eq!(String::from(Cell::DontCare), "x");
        assert_eq!(Cell::try_from("x".to_string()), Ok(Cell::DontCare));
        assert_eq!(Cell::try_from("X".to_string()), Ok(Cell::DontCare));
        assert_eq!(Cell::try_from("1f".to_string()), Ok(Cell::Value(31)));
        assert_eq!(Cell::try_from("0x1f".to_string()), Ok(Cell::Value(31)));
        assert!(Cell::try_from("nonsense".to_string()).is_err());
    }

    /// The drawing has to be readable in the panel it will be compared in.
    #[test]
    fn the_drawing_reads_back_as_a_waveform() {
        let pattern = pipeline3();
        let vcd = pattern.to_vcd();

        let mut dump = rtlscope_wave::Dump::open_vcd_bytes(vcd.into_bytes()).expect("is a vcd");
        let names: Vec<String> = dump.vars().map(|(name, _)| name.to_string()).collect();
        assert!(names.contains(&"rtlscope_stim.in_valid".to_string()), "{names:?}");
        assert!(names.contains(&"rtlscope_expect.out_valid".to_string()), "{names:?}");

        let var = dump.find("rtlscope_stim.in_data").expect("the drawn bus");
        dump.load(&[var]).expect("loads");
        // Column 2 begins at 20 ns, and holds `beef` until column 3.
        let at = dump.value_at(var, 25).expect("readable").expect("a value");
        assert_eq!(at.as_u64(), Some(0xbeef));
        let later = dump.value_at(var, 35).expect("readable").expect("a value");
        assert!(later.is_unknown(), "the don't-care survived as x: {later:?}");
    }

    #[test]
    fn a_port_too_wide_to_draw_is_named_rather_than_narrowed() {
        let mut pattern = pipeline3();
        pattern.drive.push(Lane::driven("wide_bus", 128));

        let refused = pattern.too_wide();
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("wide_bus"), "{refused:?}");
        assert_eq!(pattern.drivable().len(), 2, "and it is left out of the harness");
    }

    /// Leaving an expectation row blank must mean "I did not say", never
    /// "I said zero". The other way round the tool would report a design for
    /// failing a check nobody wrote.
    #[test]
    fn an_undrawn_expectation_is_not_an_expectation_of_zero() {
        let pattern = pipeline3();
        let expect = &pattern.expect[0];

        assert_eq!(expect.at(0), Cell::DontCare, "nothing was said about column 0");
        assert_eq!(expect.at(5), Cell::Value(1), "and this is what was said");

        // An input row is the other way about: undrawn is defined, not `x`.
        assert_eq!(pattern.drive[0].at(0), Cell::Value(0));
    }

    #[test]
    fn a_verdict_says_what_held_and_what_did_not() {
        let text = r#"{"checked": 12, "failures": [
            {"cycle": 5, "time_ns": 55.0, "port": "out_valid", "expected": "1", "got": "0"}
        ]}"#;
        let verdict: Verdict = serde_json::from_str(text).expect("reads");

        assert!(!verdict.passed());
        assert_eq!(verdict.summary(), "12 check(s), 1 failed");
        assert_eq!(verdict.failures[0].cycle, 5);
    }

    #[test]
    fn vcd_identifiers_do_not_repeat() {
        let ids: Vec<String> = (0..200).map(vcd_id).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
    }
}
