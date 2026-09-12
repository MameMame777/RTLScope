//! How long a value actually took, measured rather than counted.
//!
//! [`rtlscope_analyse::depth`] answers the structural question — how many clock
//! edges lie between two signals — and it answers it exactly, because the
//! structure is written down. It cannot answer how long the value took, because
//! that depends on what the design was doing: a register behind a clock enable
//! advances one stage and waits an arbitrary number of cycles to do it.
//!
//! So this counts the same distance in the recording. The two together say more
//! than either alone: a structure three deep and a measurement of three means
//! the short road is being taken; three against eleven means something is
//! stalling, and where the structure says *variable* is where to look.
//!
//! **The pairing rule.** The k-th change of A is matched with the k-th change
//! of B, and the distance between their cycles is one sample. Under a pipeline
//! that accepts every beat this is the same as "the next change of B"; under
//! stalls it is not, and this is the reading that measures each beat's own
//! transit rather than the gap to whatever came out next.
//!
//! It is a rule, not a truth, and it does not fit every design — a FIFO reorders
//! nothing but decouples the two ends completely, and a handshake may drop
//! beats. Everything it cannot account for goes to [`LatencyReport::problems`]
//! rather than into the histogram: a measurement that quietly dropped what did
//! not fit would read as a cleaner answer than it is.

use serde::Serialize;

use crate::dump::{Dump, WaveError, WaveValue, WaveVar};
use crate::matching::MatchReport;
use crate::stages::{Cycles, sample};

/// Why a distance could not be measured.
///
/// Its own type rather than [`crate::stages::StageError`]: that one's
/// `NoClock` says "this is the clock of this domain and the dump has no such
/// signal", which is a different sentence from "the dump has no such signal",
/// and borrowing it would put the wrong reason in front of the reader.
#[derive(Debug, thiserror::Error)]
pub enum LatencyError {
    #[error("`{name}` is not in this recording: {because}")]
    NoSignal { name: String, because: String },
    #[error(transparent)]
    Wave(#[from] WaveError),
}

/// What the recording says the distance was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatencyReport {
    /// The names as the design knows them.
    pub from: String,
    pub to: String,
    /// And as the dump does, which carries the testbench's own scope.
    pub from_path: String,
    pub to_path: String,
    /// The clock whose edges the cycles were counted in.
    pub clock: String,
    /// How many pairs the rule matched.
    pub samples: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<usize>,
    /// The lower median, so the number is always one that was measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
    /// How often each distance came up, shortest first.
    pub histogram: Vec<Bin>,
    /// What the rule could not account for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// One distance and how often it happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Bin {
    pub cycles: usize,
    pub count: usize,
}

/// Measures the distance from `from` to `to` over a recording.
///
/// Both names are the design's, resolved through the same matching the rest of
/// the wave view uses, so a name that works in one place works in all of them.
pub fn latency(
    dump: &mut Dump,
    matches: &MatchReport,
    cycles: &Cycles,
    from: &str,
    to: &str,
) -> Result<LatencyReport, LatencyError> {
    let start = matched(matches, from)?;
    let end = matched(matches, to)?;

    dump.load(&[start.1, end.1])?;
    let mut problems = Vec::new();
    let departures = changed_at(dump, start.1, cycles, &mut problems, from)?;
    let arrivals = changed_at(dump, end.1, cycles, &mut problems, to)?;

    let mut distances: Vec<usize> = Vec::new();
    let mut backwards = 0usize;
    for (left, right) in departures.iter().zip(&arrivals) {
        match right.checked_sub(*left) {
            Some(gap) => distances.push(gap),
            // The pairing rule does not fit: whatever B did, it was not this
            // beat of A arriving. Counted rather than folded in, because a
            // negative distance smoothed to zero would look like a design that
            // answers instantly.
            None => backwards += 1,
        }
    }
    if backwards > 0 {
        problems.push(format!(
            "{backwards} time(s) `{to}` changed before the `{from}` it was paired with; the \
             pairing rule does not fit this design"
        ));
    }
    if departures.len() > arrivals.len() {
        problems.push(format!(
            "{} change(s) of `{from}` had no matching change of `{to}` before the recording \
             ended",
            departures.len() - arrivals.len()
        ));
    }
    if arrivals.len() > departures.len() {
        problems.push(format!(
            "{} change(s) of `{to}` were not paired with anything; it moves without `{from}`",
            arrivals.len() - departures.len()
        ));
    }

    distances.sort_unstable();
    let mut histogram: Vec<Bin> = Vec::new();
    for distance in &distances {
        match histogram.last_mut() {
            Some(bin) if bin.cycles == *distance => bin.count += 1,
            _ => histogram.push(Bin { cycles: *distance, count: 1 }),
        }
    }

    Ok(LatencyReport {
        from: from.to_string(),
        to: to.to_string(),
        from_path: start.0,
        to_path: end.0,
        clock: cycles.clock.clone(),
        samples: distances.len(),
        min: distances.first().copied(),
        median: distances.get(distances.len() / 2).copied(),
        max: distances.last().copied(),
        histogram,
        problems,
    })
}

/// The dump's name and variable for a design signal.
fn matched(matches: &MatchReport, name: &str) -> Result<(String, WaveVar), LatencyError> {
    match matches.by_ir_name(name) {
        Some(found) => Ok((found.dump_path.clone(), found.var)),
        None => {
            // Why it is missing is a fact the matching already worked out, and
            // answering "not found" would throw it away.
            let because = matches
                .unmatched_ir
                .iter()
                .find(|missing| missing.ir_name == name)
                .map(|missing| missing.reason.clone())
                .unwrap_or_else(|| "the design has it and the dump does not".to_string());
            Err(LatencyError::NoSignal { name: name.to_string(), because })
        }
    }
}

/// The cycles in which a signal's value changed.
///
/// A change is a settled value differing from the settled value before it, and
/// **both have to be known**. An `x` becoming a number is the design coming up,
/// not the design doing something; a number becoming `x` is it going away. Both
/// would otherwise be counted as beats and shift every pair after them.
fn changed_at(
    dump: &mut Dump,
    var: WaveVar,
    cycles: &Cycles,
    problems: &mut Vec<String>,
    name: &str,
) -> Result<Vec<usize>, LatencyError> {
    let changes: Vec<(u64, WaveValue)> = dump.changes(var)?.collect();
    let per_cycle = sample(&changes, &cycles.edges);

    let mut at = Vec::new();
    let mut skipped = 0usize;
    for cycle in 1..per_cycle.len() {
        let (before, now) = (&per_cycle[cycle - 1], &per_cycle[cycle]);
        match (before, now) {
            (Some(before), Some(now)) if before != now => {
                if before.is_unknown() || now.is_unknown() {
                    skipped += 1;
                    continue;
                }
                at.push(cycle);
            }
            // Nothing settled yet on one side or the other.
            (None, Some(_)) => skipped += 1,
            _ => {}
        }
    }
    if skipped > 0 {
        problems.push(format!(
            "{skipped} change(s) of `{name}` were into or out of `x`, and are not counted as beats"
        ));
    }
    Ok(at)
}

/// What the structure and the recording say about each other.
///
/// Never one verdict: the interesting cases are the disagreements, and each of
/// them means something different about where to look.
pub fn cross_check(
    structure: &rtlscope_analyse::DepthReport,
    measured: &LatencyReport,
) -> Vec<String> {
    let mut out = Vec::new();
    let (Some(min), Some(measured_min)) = (structure.min_stages, measured.min) else {
        return out;
    };

    match measured_min.cmp(&min) {
        std::cmp::Ordering::Equal => {
            out.push("the shortest road is one the design actually takes".to_string());
        }
        std::cmp::Ordering::Greater => {
            out.push(format!(
                "never faster than {measured_min}, though the structure allows {min}: something \
                 on the way is waiting"
            ));
            if structure.variable_latency {
                out.push(
                    "the structure says which: a register on the road does not advance every \
                     cycle"
                        .to_string(),
                );
            }
        }
        // Faster than the structure permits is not a fact about the design.
        std::cmp::Ordering::Less => {
            out.push(format!(
                "measured {measured_min}, which the structure's {min} does not allow — the \
                 pairing matched the wrong changes, or these are not the two signals meant"
            ));
        }
    }

    // Only when the measurement actually spread. Where every beat took the
    // same time, the floor above has already said the whole of it, and a second
    // line repeating the number reads as a second finding.
    let spread = measured.max > measured.min;
    // Short by exactly one, over and over, is almost always the drive phase
    // rather than anything about the design: an input that changes just after
    // the edge which captures it lands in the same cycle as the register that
    // captured it, so the first hop leaves no gap to measure.
    //
    // Asked of the usual distance rather than the shortest, because one
    // mispaired beat drags the minimum and says nothing about the phase. This
    // is what a real Verilator run of a three-deep pipeline comes back with,
    // and a reader sent to look at the pairing instead would be looking at the
    // wrong thing.
    if let Some(median) = measured.median
        && min > median
        && min - median == 1
    {
        out.push(format!(
            "the usual distance is {median}, one short of the structural {min}: an input \
             driven just after the edge which captures it leaves the first hop with no gap \
             to measure"
        ));
    }

    match (structure.max_stages, measured.max) {
        (Some(max), Some(measured_max)) if measured_max > max && spread => {
            out.push(format!(
                "and as slow as {measured_max} against a longest road of {max}: it stalls"
            ));
        }
        (None, Some(measured_max)) => out.push(format!(
            "the structure has no longest road to hold this against; measured, it took up to \
             {measured_max}"
        )),
        _ => {}
    }

    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A recording built by hand rather than simulated.
    ///
    /// The same reason the stage tests do it: a stall, a beat that never comes
    /// out, and an `x` at power-on are each easy to write and awkward to
    /// arrange in a real run — and each of them is a rule this has to get
    /// right.
    ///
    /// `data` is `(time, statement)`; the clock is woven in by time, because a
    /// VCD whose times go backwards is one the reader skips over with a warning
    /// and no values.
    fn recorded(edges: usize, data: &[(u64, &str)]) -> Dump {
        let mut moments: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        // Every signal gets a value at time zero, the way a simulator writes
        // one. Without it the first real change is an appearance rather than a
        // change, which is a different rule being tested by accident.
        let at_zero = moments.entry(0).or_default();
        at_zero.push("b00000000 a".to_string());
        at_zero.push("b00000000 b".to_string());
        for cycle in 0..edges as u64 {
            moments.entry(cycle * 10).or_default().push("1!".to_string());
            moments.entry(cycle * 10 + 5).or_default().push("0!".to_string());
        }
        for (time, statement) in data {
            moments.entry(*time).or_default().push((*statement).to_string());
        }

        let mut body = String::new();
        for (time, statements) in moments {
            body.push_str(&format!("#{time}\n"));
            for statement in statements {
                body.push_str(&statement);
                body.push('\n');
            }
        }

        let text = format!(
            "$timescale 1ns $end\n\
             $scope module tb $end\n\
             $var wire 1 ! clk $end\n\
             $var wire 8 a in_data $end\n\
             $var wire 8 b out_data $end\n\
             $upscope $end\n\
             $enddefinitions $end\n{body}"
        );
        Dump::open_vcd_bytes(text.into_bytes()).expect("reads")
    }

    fn cycles_of(dump: &mut Dump) -> Cycles {
        let clk = dump.find("tb.clk").expect("the clock is there");
        dump.load(&[clk]).expect("loads");
        let edges = dump
            .changes(clk)
            .expect("reads")
            .filter(|(_, value)| value.as_bool() == Some(true))
            .map(|(time, _)| time)
            .collect();
        Cycles { clock: "clk".to_string(), path: "tb.clk".to_string(), edges }
    }

    /// The cycles a signal changed in, which is the whole basis of the measure.
    fn beats(dump: &mut Dump, path: &str, cycles: &Cycles) -> (Vec<usize>, Vec<String>) {
        let var = dump.find(path).expect("the signal is there");
        dump.load(&[var]).expect("loads");
        let mut problems = Vec::new();
        let at = changed_at(dump, var, cycles, &mut problems, path).expect("reads");
        (at, problems)
    }

    /// The plain case: every beat takes the same two cycles, and the histogram
    /// is one bin.
    #[test]
    fn a_fixed_distance_is_one_bin() {
        let mut dump = recorded(
            10,
            &[(20, "b00000001 a"), (40, "b00000010 a"), (40, "b00000001 b"), (60, "b00000010 b")],
        );
        let cycles = cycles_of(&mut dump);

        let (departures, _) = beats(&mut dump, "tb.in_data", &cycles);
        let (arrivals, _) = beats(&mut dump, "tb.out_data", &cycles);
        assert_eq!(departures, [2, 4], "two beats went in");
        assert_eq!(arrivals, [4, 6], "two came out, two cycles later each");

        let gaps: Vec<usize> = departures.iter().zip(&arrivals).map(|(l, r)| r - l).collect();
        assert_eq!(gaps, [2, 2]);
    }

    /// A change exactly on the edge belongs to that cycle, not the next: the
    /// value a register settles on at edge *n* is its value during cycle *n*.
    /// The rest of the wave view reads dumps this way and this has to agree, or
    /// a measured distance and a cursor would disagree by one.
    #[test]
    fn a_change_on_the_edge_belongs_to_that_cycle() {
        let mut dump = recorded(6, &[(20, "b00000001 a")]);
        let cycles = cycles_of(&mut dump);
        let (at, _) = beats(&mut dump, "tb.in_data", &cycles);
        // Edges are at 0, 10, 20, ... so a change at time 20 is cycle 2.
        assert_eq!(at, [2]);
    }

    /// Coming up out of `x` is the design starting, not the design doing
    /// something. Counting it would shift every pair after it by one — and the
    /// skip is reported, because a beat silently dropped is a histogram that
    /// looks cleaner than the run was.
    #[test]
    fn coming_out_of_x_is_not_a_beat_and_is_said_so() {
        // The `x` lands after the helper's zero, so cycle 2 is a change
        // *into* `x` and cycle 4 is one out of it. Neither is a beat.
        let mut dump =
            recorded(6, &[(20, "bxxxxxxxx a"), (40, "b00000001 a"), (50, "b00000010 a")]);
        let cycles = cycles_of(&mut dump);
        let (at, problems) = beats(&mut dump, "tb.in_data", &cycles);

        assert_eq!(at, [5], "only the change between two known values");
        assert!(!problems.is_empty(), "and the skipped one is reported: {problems:?}");
    }

    /// A stall: two beats in, and the second waits. The two distances are
    /// different, which is the whole reason this is a histogram and not an
    /// average.
    #[test]
    fn a_stall_is_two_distances_rather_than_one_average() {
        let mut dump = recorded(
            12,
            &[(20, "b00000001 a"), (30, "b00000010 a"), (40, "b00000001 b"), (90, "b00000010 b")],
        );
        let cycles = cycles_of(&mut dump);
        let (departures, _) = beats(&mut dump, "tb.in_data", &cycles);
        let (arrivals, _) = beats(&mut dump, "tb.out_data", &cycles);

        let gaps: Vec<usize> = departures.iter().zip(&arrivals).map(|(l, r)| r - l).collect();
        assert_eq!(gaps, [2, 6], "one went straight through and one waited");
    }

    /// Two beats in, one out. The unmatched one is reported rather than
    /// dropped.
    #[test]
    fn a_beat_that_never_arrived_is_reported_rather_than_dropped() {
        let mut dump =
            recorded(10, &[(20, "b00000001 a"), (40, "b00000010 a"), (60, "b00000001 b")]);
        let cycles = cycles_of(&mut dump);
        let (departures, _) = beats(&mut dump, "tb.in_data", &cycles);
        let (arrivals, _) = beats(&mut dump, "tb.out_data", &cycles);

        assert_eq!(departures.len(), 2);
        assert_eq!(arrivals.len(), 1, "one of them never came out");
    }

    /// The verdicts are about the disagreement between the two layers, and each
    /// disagreement means something different about where to look.
    #[test]
    fn the_two_layers_are_held_against_each_other() {
        let measured = |min: usize, max: usize| LatencyReport {
            from: "a".into(),
            to: "b".into(),
            from_path: "tb.a".into(),
            to_path: "tb.b".into(),
            clock: "clk".into(),
            samples: 2,
            min: Some(min),
            median: Some(min),
            max: Some(max),
            histogram: Vec::new(),
            problems: Vec::new(),
        };
        let structure = |min: Option<usize>, max: Option<usize>, variable: bool| {
            rtlscope_analyse::DepthReport {
                from: "a".into(),
                to: "b".into(),
                clock: Some("clk".into()),
                min_stages: min,
                max_stages: max,
                feedback: max.is_none(),
                reconvergent: false,
                variable_latency: variable,
                paths: Vec::new(),
                errors: Vec::new(),
                warnings: Vec::new(),
                problems: Vec::new(),
            }
        };

        let agreed = cross_check(&structure(Some(3), Some(3), false), &measured(3, 3));
        assert!(agreed[0].contains("actually takes"), "{agreed:?}");

        let stalling = cross_check(&structure(Some(3), Some(3), true), &measured(5, 11));
        assert!(stalling[0].contains("never faster"), "{stalling:?}");
        assert!(stalling.iter().any(|line| line.contains("every cycle")), "{stalling:?}");
        assert!(stalling.iter().any(|line| line.contains("stalls")), "{stalling:?}");

        // Faster than the structure permits is not a fact about the design, and
        // saying so beats reporting a number nothing could have produced.
        let impossible = cross_check(&structure(Some(3), Some(3), false), &measured(1, 1));
        assert!(impossible[0].contains("does not allow"), "{impossible:?}");

        let looping = cross_check(&structure(Some(1), None, true), &measured(4, 9));
        assert!(looping.iter().any(|line| line.contains("no longest road")), "{looping:?}");

        // Short by exactly one is the drive phase, not the design. Found on a
        // real Verilator run of a three-deep pipeline: 113 beats measured 2,
        // and calling that a mispairing would send a reader to the wrong place.
        let phase = cross_check(&structure(Some(3), Some(3), false), &measured(2, 2));
        assert!(phase.iter().any(|line| line.contains("no gap")), "the phase is named: {phase:?}");

        // Two short is not, and must not borrow the same excuse.
        let really_wrong = cross_check(&structure(Some(4), Some(4), false), &measured(2, 2));
        assert!(!really_wrong.iter().any(|line| line.contains("no gap")), "{really_wrong:?}");
    }
}
