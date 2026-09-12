//! What each pipeline stage was carrying, cycle by cycle.
//!
//! `rtlscope pipeline` says which registers sit at which depth; a dump says what
//! those registers held. Put one against the other on a single time axis and
//! the result is the diagram a pipeline is always explained with: stages down
//! the side, cycles across, and a mark where the stage had something in it.
//!
//! The mark is the part that has to be honest, because *occupancy is not
//! recorded anywhere*. Nothing in a waveform says "stage 2 holds a pixel"; that
//! is a claim about what the design meant. So each row looks for the bit that
//! carries the claim — a one-bit register at that stage with `valid` in its
//! name — and reports which one it used. Where there is no such bit the row
//! falls back to whether anything at the stage changed on that edge, and
//! reports *that* instead. The two mean different things, and a diagram that
//! ran them together would be a guess wearing a picture's clothes.
//!
//! Everything is measured at rising edges of the domain's own clock, taken from
//! the dump rather than assumed: a register's value during cycle *n* is what it
//! settled on at edge *n*, which is exactly what [`sample`] returns.

use rtlscope_analyse::pipeline::{DomainDepth, Stage};
use serde::Serialize;

use crate::dump::{Dump, WaveError, WaveValue, WaveVar};
use crate::matching::MatchReport;

/// How many cycles one view may cover.
///
/// Past this there is more than one cycle per column of anything drawing it,
/// and a cycle diagram that cannot show cycles is not one.
pub const MAX_WINDOW: usize = 4096;

/// How many registers a row without a valid bit will watch.
const MAX_WATCHED: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum StageError {
    #[error("`{0}` is the clock of this domain, and no signal in the dump matches it")]
    NoClock(String),
    #[error("`{0}` never rises in this dump, so there are no cycles to lay anything against")]
    NoEdges(String),
    #[error(transparent)]
    Wave(#[from] WaveError),
}

/// A clock's rising edges, which is what "cycle 40" means.
#[derive(Debug, Clone)]
pub struct Cycles {
    /// The name the design knows the clock by.
    pub clock: String,
    /// The name the dump knows it by.
    pub path: String,
    /// The moment of each rising edge; the index is the cycle number.
    pub edges: Vec<u64>,
}

impl Cycles {
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    pub fn at(&self, cycle: usize) -> Option<u64> {
        self.edges.get(cycle).copied()
    }

    /// The cycle in force at a moment: the last edge at or before it.
    ///
    /// `None` before the first edge, since there is no cycle 0 yet.
    pub fn cycle_at(&self, time: u64) -> Option<usize> {
        self.edges.partition_point(|edge| *edge <= time).checked_sub(1)
    }
}

/// Finds a domain's clock in the dump and reads its rising edges.
pub fn cycles(dump: &mut Dump, matches: &MatchReport, clock: &str) -> Result<Cycles, StageError> {
    let matched =
        matches.by_ir_name(clock).ok_or_else(|| StageError::NoClock(clock.to_string()))?;
    let (var, path) = (matched.var, matched.dump_path.clone());
    dump.load(&[var])?;

    let mut edges = Vec::new();
    let mut was_high = false;
    for (time, value) in dump.changes(var)? {
        // An undriven clock is not an edge; it is the absence of one, and the
        // next real 1 after it counts as a rise.
        let Some(high) = value.as_bool() else {
            was_high = false;
            continue;
        };
        if high && !was_high {
            edges.push(time);
        }
        was_high = high;
    }
    if edges.is_empty() {
        return Err(StageError::NoEdges(path));
    }
    Ok(Cycles { clock: clock.to_string(), path, edges })
}

/// What one stage did in one cycle.
///
/// What `Busy` means depends on the row's [`Basis`], which says so: with a
/// valid bit it means the stage held something, and without one it means
/// something at the stage changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cell {
    /// Nothing to read: no register of this stage is in the dump, or the dump
    /// has not reached its first value yet.
    Blank,
    /// Nothing happening.
    Idle,
    Busy,
    /// Busy, and carrying what it carried last cycle — a stall.
    Held,
    /// At least one bit was `x` or `z`.
    Unknown,
}

impl Cell {
    /// The character this reads as in a text diagram.
    pub fn glyph(self) -> char {
        match self {
            Cell::Blank => ' ',
            Cell::Idle => '.',
            Cell::Busy => '#',
            Cell::Held => '~',
            Cell::Unknown => '?',
        }
    }
}

/// How a row decided what its cells mean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "from", rename_all = "lowercase")]
pub enum Basis {
    /// A one-bit register at this stage whose name says it carries a valid.
    Valid { signal: String },
    /// No such bit, so a cell says whether anything watched here changed.
    Activity { watching: Vec<String> },
    /// Nothing at this stage is in the dump at all.
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StageRow {
    pub stage: usize,
    /// How many registers the analysis put at this stage.
    pub registers: usize,
    /// How many of them the dump has.
    pub matched: usize,
    pub basis: Basis,
    /// The widest register here, whose value fills the cells.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    pub cells: Vec<Cell>,
    /// What the payload held, one per cell, when there is a payload.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// How many cells were busy or held, and how many were held.
    pub occupied: usize,
    pub held: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StageView {
    pub clock: String,
    pub clock_path: String,
    /// How many stages deep the domain is.
    pub depth: usize,
    /// How many rising edges the whole dump has.
    pub cycles: usize,
    /// The first cycle this window covers.
    pub first: usize,
    /// The moment of each cycle in the window.
    pub times: Vec<u64>,
    pub rows: Vec<StageRow>,
    /// Everything that could not be read, said rather than left out.
    pub problems: Vec<String>,
}

impl StageView {
    pub fn len(&self) -> usize {
        self.times.len()
    }

    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }
}

/// Which cycles to lay out, and what to read them from.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// The first cycle, and how many. Cycle numbers, not times: this is a cycle
    /// diagram, and a window wider than [`MAX_WINDOW`] is trimmed and said to
    /// have been.
    pub first: usize,
    pub len: usize,
    /// A stage's valid bit, named rather than guessed at, as `(stage, signal)`.
    ///
    /// The heuristic knows the common spellings and no more. A design that
    /// calls its valid `v3` would otherwise get the movement fallback with no
    /// way to correct it — and widening the guess until `v3` matched would
    /// start calling things valids that are not.
    pub valid: Vec<(usize, String)>,
    /// A stage's payload, as `(stage, signal)`.
    ///
    /// Worth naming for the same reason: whether a cell reads as a stall is
    /// decided by whether the payload repeated, so a payload chosen by width
    /// alone can make a stage look stuck when it is not.
    pub payload: Vec<(usize, String)>,
}

impl Layout {
    /// The first `len` cycles from `first`, with the signals guessed at.
    pub fn window(first: usize, len: usize) -> Self {
        Layout { first, len, valid: Vec::new(), payload: Vec::new() }
    }
}

/// Lays a domain's stages against a window of the dump's cycles.
pub fn occupancy(
    dump: &mut Dump,
    matches: &MatchReport,
    domain: &DomainDepth,
    cycles: &Cycles,
    layout: &Layout,
) -> StageView {
    let mut problems = Vec::new();

    let total = cycles.edges.len();
    let first = layout.first.min(total.saturating_sub(1));
    let asked = layout.len.max(1);
    let len = asked.min(MAX_WINDOW).min(total - first);
    if asked > len {
        problems.push(format!(
            "{asked} cycle(s) were asked for and {len} drawn — the dump has {total}, and a \
             window is capped at {MAX_WINDOW} because past that a column covers more than one \
             cycle and a cycle diagram stops being one."
        ));
    }

    // One edge before the window as well, so the first column can say whether
    // anything moved *into* it rather than starting blind.
    let back = usize::from(first > 0);
    let probe: Vec<u64> = cycles.edges[first - back..first + len].to_vec();

    // Everything is resolved and loaded before anything is sampled: one pass
    // over the file rather than one per register.
    let plans: Vec<Plan> = domain
        .stages
        .iter()
        .map(|stage| {
            let named = |chosen: &'_ [(usize, String)]| {
                chosen.iter().find(|(index, _)| *index == stage.index).map(|(_, name)| name.clone())
            };
            plan_stage(matches, stage, named(&layout.valid), named(&layout.payload), &mut problems)
        })
        .collect();
    let mut wanted: Vec<WaveVar> = Vec::new();
    for plan in &plans {
        wanted.extend(plan.valid.iter().map(|(_, var)| *var));
        wanted.extend(plan.payload.iter().map(|(_, var)| *var));
        wanted.extend(plan.watched.iter().map(|(_, var)| *var));
    }
    if let Err(error) = dump.load(&wanted) {
        problems.push(format!("the dump would not give up its values: {error}"));
    }

    let rows =
        plans.iter().map(|plan| fill(dump, plan, &probe, back, &mut problems)).collect::<Vec<_>>();

    StageView {
        clock: cycles.clock.clone(),
        clock_path: cycles.path.clone(),
        depth: domain.depth,
        cycles: total,
        first,
        times: cycles.edges[first..first + len].to_vec(),
        rows,
        problems,
    }
}

/// What one row will read, decided before anything is loaded.
struct Plan {
    stage: usize,
    registers: usize,
    matched: usize,
    valid: Option<(String, WaveVar)>,
    payload: Option<(String, WaveVar)>,
    /// Only when there is no valid: the registers whose movement stands in for
    /// occupancy.
    watched: Vec<(String, WaveVar)>,
}

fn plan_stage(
    matches: &MatchReport,
    stage: &Stage,
    named_valid: Option<String>,
    named_payload: Option<String>,
    problems: &mut Vec<String>,
) -> Plan {
    // Widths come along because the valid bit is one bit and the payload is
    // more than one, and that is most of how they are told apart.
    let present: Vec<(String, WaveVar, u32)> = stage
        .registers
        .iter()
        .filter_map(|name| matches.by_ir_name(name).map(|m| (name.clone(), m.var, m.width)))
        .collect();

    if present.is_empty() {
        if !stage.registers.is_empty() {
            problems.push(format!(
                "stage {}: none of its {} register(s) are in the dump, so the row is blank",
                stage.index,
                stage.registers.len()
            ));
        }
        return Plan {
            stage: stage.index,
            registers: stage.registers.len(),
            matched: 0,
            valid: None,
            payload: None,
            watched: Vec::new(),
        };
    }

    // A valid the caller named beats anything guessed at, and a name that does
    // not resolve is said so rather than quietly falling back.
    let mut valid = None;
    if let Some(wanted) = &named_valid {
        match matches.by_ir_name(wanted) {
            Some(found) if found.width == 1 => valid = Some((wanted.clone(), found.var)),
            Some(found) => problems.push(format!(
                "stage {}: `{wanted}` is {} bits, so it cannot be the bit that says the stage \
                 is carrying something",
                stage.index, found.width
            )),
            None => problems.push(format!(
                "stage {}: `{wanted}` is not in the dump, so the row falls back to what it \
                 would have shown anyway",
                stage.index
            )),
        }
    }

    // Otherwise the valid bit: one bit wide, and named like one.
    let mut candidates: Vec<&(String, WaveVar, u32)> =
        present.iter().filter(|(name, _, width)| *width == 1 && reads_as_valid(name)).collect();
    // Shortest name first, then alphabetical: of `d1_valid` and `d1_valid_q`,
    // the plain one is the stage's own.
    candidates.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));
    if valid.is_none() {
        valid = candidates.first().map(|(name, var, _)| (name.clone(), *var));
        if candidates.len() > 1 {
            let others: Vec<&str> =
                candidates.iter().skip(1).take(4).map(|(name, _, _)| name.as_str()).collect();
            problems.push(format!(
                "stage {}: `{}` was taken as the valid; {} other(s) here are named like one too \
                 ({})",
                stage.index,
                candidates[0].0,
                candidates.len() - 1,
                others.join(", ")
            ));
        }
    }

    // The payload, named or else the widest register that is not the valid.
    let mut payload = None;
    if let Some(wanted) = &named_payload {
        match matches.by_ir_name(wanted) {
            Some(found) => payload = Some((wanted.clone(), found.var)),
            None => problems.push(format!(
                "stage {}: `{wanted}` is not in the dump, so the widest register here is \
                 shown instead",
                stage.index
            )),
        }
    }
    if payload.is_none() {
        let chosen = valid.as_ref().map(|(name, _)| name.as_str());
        let mut wide: Vec<&(String, WaveVar, u32)> = present
            .iter()
            .filter(|(name, _, width)| *width > 1 && Some(name.as_str()) != chosen)
            .collect();
        wide.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        payload = wide.first().map(|(name, var, _)| (name.clone(), *var));
    }

    let watched = if valid.is_some() {
        Vec::new()
    } else {
        let mut all: Vec<(String, WaveVar)> =
            present.iter().map(|(name, var, _)| (name.clone(), *var)).collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        if all.len() > MAX_WATCHED {
            problems.push(format!(
                "stage {}: nothing here is named like a valid, so the row shows movement \
                 instead — of the first {MAX_WATCHED} of its {} registers",
                stage.index,
                all.len()
            ));
            all.truncate(MAX_WATCHED);
        }
        all
    };

    Plan {
        stage: stage.index,
        registers: stage.registers.len(),
        matched: present.len(),
        valid,
        payload,
        watched,
    }
}

/// Reads one row's cells out of the dump.
fn fill(
    dump: &Dump,
    plan: &Plan,
    probe: &[u64],
    back: usize,
    problems: &mut Vec<String>,
) -> StageRow {
    let columns = probe.len() - back;
    let read = |var: WaveVar, problems: &mut Vec<String>| -> Vec<Option<WaveValue>> {
        match dump.changes(var) {
            Ok(changes) => {
                let changes: Vec<(u64, WaveValue)> = changes.collect();
                sample(&changes, probe)
            }
            Err(error) => {
                problems.push(format!("stage {}: {error}", plan.stage));
                vec![None; probe.len()]
            }
        }
    };

    let payload = plan.payload.as_ref().map(|(_, var)| read(*var, problems));
    let mut cells = vec![Cell::Blank; columns];

    match &plan.valid {
        Some((_, var)) => {
            let valid = read(*var, problems);
            for (column, cell) in cells.iter_mut().enumerate() {
                let at = column + back;
                *cell = match &valid[at] {
                    None => Cell::Blank,
                    Some(value) if value.is_unknown() => Cell::Unknown,
                    Some(value) => match value.as_bool() {
                        Some(false) => Cell::Idle,
                        // Busy, unless the payload and the valid both repeat —
                        // which is a stage that did not advance.
                        Some(true) => {
                            let stalled = at > 0
                                && valid[at - 1].as_ref().and_then(WaveValue::as_bool)
                                    == Some(true)
                                && payload
                                    .as_ref()
                                    .is_some_and(|values| values[at] == values[at - 1]);
                            if stalled { Cell::Held } else { Cell::Busy }
                        }
                        None => Cell::Unknown,
                    },
                };
            }
        }
        None if !plan.watched.is_empty() => {
            let tracks: Vec<Vec<Option<WaveValue>>> =
                plan.watched.iter().map(|(_, var)| read(*var, problems)).collect();
            for (column, cell) in cells.iter_mut().enumerate() {
                let at = column + back;
                let unknown =
                    tracks.iter().any(|t| t[at].as_ref().is_some_and(WaveValue::is_unknown));
                let anything = tracks.iter().any(|t| t[at].is_some());
                let moved = at > 0 && tracks.iter().any(|t| t[at] != t[at - 1]);
                *cell = match (anything, unknown, moved) {
                    (false, _, _) => Cell::Blank,
                    (_, true, _) => Cell::Unknown,
                    (_, _, true) => Cell::Busy,
                    _ => Cell::Idle,
                };
            }
        }
        None => {}
    }

    let values = payload
        .map(|values| {
            values[back..]
                .iter()
                .map(|value| value.as_ref().map(shown).unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();

    let occupied = cells.iter().filter(|c| matches!(c, Cell::Busy | Cell::Held)).count();
    let held = cells.iter().filter(|c| matches!(c, Cell::Held)).count();

    StageRow {
        stage: plan.stage,
        registers: plan.registers,
        matched: plan.matched,
        basis: match (&plan.valid, plan.watched.is_empty()) {
            (Some((name, _)), _) => Basis::Valid { signal: name.clone() },
            (None, false) => {
                Basis::Activity { watching: plan.watched.iter().map(|(n, _)| n.clone()).collect() }
            }
            (None, true) => Basis::Absent,
        },
        payload: plan.payload.as_ref().map(|(name, _)| name.clone()),
        cells,
        values,
        occupied,
        held,
    }
}

fn shown(value: &WaveValue) -> String {
    match value.as_u64() {
        Some(number) if value.width() == 1 => number.to_string(),
        Some(number) => format!("0x{number:x}"),
        None => value.bit_string(),
    }
}

/// Whether a name reads like the bit that says a stage is carrying something.
///
/// Both conventions are in use and neither is a suffix rule: `d1_valid` puts
/// the word last and `valid_d1` puts it first, and a design that only matched
/// one of them would fall back to showing movement on half the pipelines it
/// met. So the name is split on underscores and any part that *is* the word
/// counts — which also takes `tvalid` and leaves `validate` alone.
fn reads_as_valid(name: &str) -> bool {
    let leaf = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
    leaf.split('_')
        .any(|part| part == "valid" || part == "vld" || part == "dv" || part.ends_with("valid"))
}

/// What a signal held at each of these moments — the last change at or before
/// each one.
///
/// Both lists are in time order, so this walks them together in one pass rather
/// than searching for each moment: a hundred thousand cycles against a hundred
/// thousand changes costs one traversal, not a hundred thousand binary
/// searches. `None` means the signal had not changed for the first time yet,
/// which is not the same as its being zero.
pub fn sample(changes: &[(u64, WaveValue)], at: &[u64]) -> Vec<Option<WaveValue>> {
    let mut out = Vec::with_capacity(at.len());
    let mut next = 0usize;
    let mut current: Option<WaveValue> = None;
    for moment in at {
        while next < changes.len() && changes[next].0 <= *moment {
            current = Some(changes[next].1.clone());
            next += 1;
        }
        out.push(current.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(value: u64, width: u32) -> WaveValue {
        WaveValue::Bits { value, width }
    }

    #[test]
    fn a_moment_takes_the_last_change_at_or_before_it() {
        let changes = vec![(0, bits(0, 8)), (10, bits(1, 8)), (30, bits(2, 8))];
        let taken = sample(&changes, &[5, 10, 20, 30, 40]);
        let numbers: Vec<Option<u64>> =
            taken.iter().map(|v| v.as_ref().and_then(WaveValue::as_u64)).collect();
        // At 10 the change has happened: a register's new value is what it
        // settles on at the edge, not one cycle later.
        assert_eq!(numbers, vec![Some(0), Some(1), Some(1), Some(2), Some(2)]);
    }

    #[test]
    fn a_moment_before_the_first_change_holds_nothing() {
        let changes = vec![(100, bits(1, 1))];
        let taken = sample(&changes, &[0, 50, 100]);
        assert_eq!(taken[0], None, "not zero: nothing");
        assert_eq!(taken[1], None);
        assert_eq!(taken[2], Some(bits(1, 1)));
    }

    #[test]
    fn sampling_nothing_gives_nothing_rather_than_panicking() {
        assert_eq!(sample(&[], &[0, 1, 2]), vec![None, None, None]);
        assert!(sample(&[(0, bits(1, 1))], &[]).is_empty());
    }

    #[test]
    fn a_valid_is_recognised_whichever_end_of_the_name_it_sits_at() {
        assert!(reads_as_valid("u_rx.d1_valid"));
        // The other half of the convention, which a suffix rule would miss.
        assert!(reads_as_valid("valid_d1"));
        assert!(reads_as_valid("m_axis_tvalid"));
        assert!(reads_as_valid("stage_vld"));
        assert!(reads_as_valid("dv"));
        // An enable decides whether a stage moves, not whether it holds
        // anything, and calling it a valid would invent occupancy.
        assert!(!reads_as_valid("d1_en"));
        assert!(!reads_as_valid("u_rx.data"));
        assert!(!reads_as_valid("validate"));
    }

    #[test]
    fn a_cycle_is_the_last_edge_at_or_before_a_moment() {
        let cycles = Cycles { clock: "clk".into(), path: "tb.clk".into(), edges: vec![10, 20, 30] };
        assert_eq!(cycles.cycle_at(9), None, "before the first edge there is no cycle");
        assert_eq!(cycles.cycle_at(10), Some(0));
        assert_eq!(cycles.cycle_at(25), Some(1));
        assert_eq!(cycles.cycle_at(1000), Some(2));
    }
}
