//! Two recordings, held against each other.
//!
//! The question this answers is the one asked after every change to an RTL: it
//! used to work, so what is different now? A waveform viewer that can only show
//! one dump leaves that to the eye, across two windows, on signals that may not
//! even be at the same place on the screen.
//!
//! Three things are said, and the order matters. **What the two do not have in
//! common** comes first, because a signal missing from one side is not a
//! difference in behaviour and reading it as one wastes the reader's afternoon.
//! Then **where they part**, per signal, earliest first. Then **what this could
//! not account for** — the budget it stopped at, a timebase it could not line
//! up — because a comparison that quietly compared less than it was asked to is
//! the worst of the three answers it could give.
//!
//! Everything is reported in the **first** dump's ticks. Two numbers for one
//! moment is how a reader ends up looking at the wrong one, and the first dump
//! is the one they were already looking at.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::dump::{Dump, WaveValue, WaveVar};

/// How many shared signals a comparison will read before it stops.
///
/// Loading a signal is the expensive part, and a dump of a real design has
/// tens of thousands. Stopping is fine; stopping quietly is not, so the number
/// left unread goes in `problems`.
pub const BUDGET: usize = 4096;

/// What two dumps have in common, and where they part.
#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    /// How many variables both recordings have.
    pub shared: usize,
    /// Paths the first has and the second does not, and the other way round.
    pub only_in_a: Vec<String>,
    pub only_in_b: Vec<String>,
    /// Every shared signal that ever differs, earliest moment first.
    pub differing: Vec<Divergence>,
    /// What this could not account for. Empty means the answer is whole.
    pub problems: Vec<String>,
}

impl Comparison {
    /// The first moment the two recordings part, over all signals.
    pub fn first(&self) -> Option<&Divergence> {
        self.differing.first()
    }

    /// True when every shared signal held the same value throughout.
    ///
    /// Not the same as the two dumps being the same: `only_in_a` may be long.
    pub fn agrees(&self) -> bool {
        self.differing.is_empty()
    }
}

/// One signal, and the first moment the two recordings disagree about it.
#[derive(Debug, Clone, Serialize)]
pub struct Divergence {
    pub path: String,
    /// In the first dump's ticks.
    pub at: u64,
    /// What each held there, written the way a waveform would show it.
    pub a: String,
    pub b: String,
}

/// Holds two dumps against each other.
///
/// Both are `&mut` because comparing means reading, and a dump reads its
/// signals on demand.
pub fn compare(a: &mut Dump, b: &mut Dump) -> Comparison {
    let mut problems = Vec::new();

    // Owned, because the paths are borrowed from the dumps and both are about
    // to be loaded into.
    let in_a: BTreeMap<String, WaveVar> =
        a.vars().map(|(path, var)| (path.to_string(), var)).collect();
    let in_b: BTreeMap<String, WaveVar> =
        b.vars().map(|(path, var)| (path.to_string(), var)).collect();

    let only_in_a: Vec<String> =
        in_a.keys().filter(|path| !in_b.contains_key(*path)).cloned().collect();
    let only_in_b: Vec<String> =
        in_b.keys().filter(|path| !in_a.contains_key(*path)).cloned().collect();

    let mut shared: Vec<(String, WaveVar, WaveVar)> = in_a
        .iter()
        .filter_map(|(path, var)| Some((path.clone(), *var, *in_b.get(path)?)))
        .collect();
    let total = shared.len();
    if total > BUDGET {
        problems.push(format!(
            "{} shared signal(s) were not read: this compares the first {BUDGET}",
            total - BUDGET
        ));
        shared.truncate(BUDGET);
    }

    // Whether the two count time the same way, decided once.
    let convert = match (a.timescale(), b.timescale()) {
        (Some(x), Some(y)) if x == y => false,
        (Some(_), Some(_)) => true,
        _ => {
            problems.push(
                "at least one dump declares no timescale, so these are raw ticks compared \
                 against raw ticks — if the two were made by different tools the moments \
                 may not line up"
                    .to_string(),
            );
            false
        }
    };

    let vars_a: Vec<WaveVar> = shared.iter().map(|(_, var, _)| *var).collect();
    let vars_b: Vec<WaveVar> = shared.iter().map(|(_, _, var)| *var).collect();
    if let Err(error) = a.load(&vars_a) {
        problems.push(format!("the first dump would not read: {error}"));
    }
    if let Err(error) = b.load(&vars_b) {
        problems.push(format!("the second dump would not read: {error}"));
    }

    let mut differing = Vec::new();
    let mut unread = 0usize;
    for (path, var_a, var_b) in &shared {
        let (Ok(left), Ok(right)) = (a.changes(*var_a), b.changes(*var_b)) else {
            unread += 1;
            continue;
        };
        let left: Vec<(u64, WaveValue)> = left.collect();
        let right: Vec<(u64, WaveValue)> =
            right.filter_map(|(at, value)| Some((in_a_ticks(at, a, b, convert)?, value))).collect();

        if let Some((at, mine, theirs)) = parts(&left, &right) {
            differing.push(Divergence { path: path.clone(), at, a: show(&mine), b: show(&theirs) });
        }
    }
    if unread > 0 {
        problems.push(format!("{unread} shared signal(s) could not be read back"));
    }

    differing.sort_by_key(|divergence| divergence.at);
    Comparison { shared: total, only_in_a, only_in_b, differing, problems }
}

/// A moment of the second dump, in the first one's ticks.
fn in_a_ticks(at: u64, a: &Dump, b: &Dump, convert: bool) -> Option<u64> {
    match convert {
        false => Some(at),
        true => a.ticks_of_ns(b.ns_of_ticks(at)?),
    }
}

/// The first moment two change lists disagree about what is held.
///
/// A merge rather than a sample: the two record their own moments, and the only
/// times worth looking at are the ones where one of them moved.
///
/// Comparison starts once **both** have recorded something. Before that one
/// side has no value rather than a different one, and calling that a difference
/// would flag every signal in a pair of dumps that merely start at different
/// moments.
fn parts(a: &[(u64, WaveValue)], b: &[(u64, WaveValue)]) -> Option<(u64, WaveValue, WaveValue)> {
    let (mut i, mut j) = (0usize, 0usize);
    let (mut here, mut there): (Option<&WaveValue>, Option<&WaveValue>) = (None, None);

    loop {
        let at = match (a.get(i).map(|(at, _)| *at), b.get(j).map(|(at, _)| *at)) {
            (Some(x), Some(y)) => x.min(y),
            (Some(x), None) => x,
            (None, Some(y)) => y,
            (None, None) => return None,
        };
        // Every change at this moment, so the last one wins: a dump may record
        // several delta cycles at one timestamp and only the settled value is
        // what anything downstream saw.
        while let Some((_, value)) = a.get(i).filter(|(when, _)| *when == at) {
            here = Some(value);
            i += 1;
        }
        while let Some((_, value)) = b.get(j).filter(|(when, _)| *when == at) {
            there = Some(value);
            j += 1;
        }
        if let (Some(here), Some(there)) = (here, there)
            && here != there
        {
            return Some((at, here.clone(), there.clone()));
        }
    }
}

/// A value the way a waveform shows it.
fn show(value: &WaveValue) -> String {
    match value {
        WaveValue::Bits { value, width: 1 } => value.to_string(),
        WaveValue::Bits { value, .. } => format!("0x{value:x}"),
        WaveValue::Wide { bits, .. } => format!("0b{bits}"),
        WaveValue::Unknown { bits } => bits.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(value: u64, width: u32) -> WaveValue {
        WaveValue::Bits { value, width }
    }

    /// The value held between two changes is the earlier one, so a signal that
    /// changes at different moments still agrees wherever both are settled.
    #[test]
    fn two_recordings_that_hold_the_same_values_never_part() {
        let a = vec![(0, bits(0, 1)), (10, bits(1, 1)), (20, bits(0, 1))];
        let b = vec![(0, bits(0, 1)), (10, bits(1, 1)), (20, bits(0, 1))];
        assert_eq!(parts(&a, &b), None);
    }

    #[test]
    fn the_moment_reported_is_the_first_one_they_disagree_about() {
        let a = vec![(0, bits(0, 1)), (10, bits(1, 1)), (30, bits(0, 1))];
        // Same until 30, where this one stays high.
        let b = vec![(0, bits(0, 1)), (10, bits(1, 1))];

        let (at, mine, theirs) = parts(&a, &b).expect("they part");
        assert_eq!(at, 30);
        assert_eq!(mine, bits(0, 1));
        assert_eq!(theirs, bits(1, 1), "the second held its last value");
    }

    /// One side starting later is not a difference in behaviour. A pair of
    /// dumps whose first moments differ would otherwise flag every signal.
    #[test]
    fn a_side_that_has_not_started_yet_is_not_a_difference() {
        let a = vec![(0, bits(1, 1)), (50, bits(0, 1))];
        let b = vec![(20, bits(1, 1)), (50, bits(0, 1))];
        assert_eq!(parts(&a, &b), None);

        // ...but once it has started, it is held to the same standard.
        let b = vec![(20, bits(0, 1))];
        let (at, _, _) = parts(&a, &b).expect("they part once both have a value");
        assert_eq!(at, 20);
    }

    /// An unknown is not a number, here as everywhere else: `x` against 0 is a
    /// difference, and one a reader very much wants to see.
    #[test]
    fn an_unknown_never_equals_a_value() {
        let a = vec![(0, bits(0, 1))];
        let b = vec![(0, WaveValue::Unknown { bits: "x".into() })];

        let (at, mine, theirs) = parts(&a, &b).expect("they part");
        assert_eq!(at, 0);
        assert_eq!(show(&mine), "0");
        assert_eq!(show(&theirs), "x");
    }

    /// Several changes at one timestamp are delta cycles; only the settled
    /// value is what anything downstream saw.
    #[test]
    fn only_the_settled_value_at_a_timestamp_is_compared() {
        let a = vec![(0, bits(0, 1)), (10, bits(1, 1)), (10, bits(0, 1))];
        let b = vec![(0, bits(0, 1)), (10, bits(0, 1))];
        assert_eq!(parts(&a, &b), None, "the glitch at 10 settled to the same value");
    }
}
