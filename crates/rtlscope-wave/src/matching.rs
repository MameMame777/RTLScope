//! Working out which signal in a dump is which net in the design.
//!
//! The two name the same wires differently. A dump records the path from
//! whatever it was told to trace — usually a testbench, so `tb.dut.u_rx.data`
//! — while the design's flattened names start at the top module, so
//! `u_rx.data`. The difference is a constant prefix, and everything else lines
//! up, so the whole job is finding that prefix and then matching by suffix.
//!
//! The prefix is inferred rather than demanded, because a caller who has to
//! know it already knows more about the dump than the tool does. But it is
//! inferred by *counting*: every candidate scope is tried, the one that
//! matches the most nets wins, and the choice comes back in the report with
//! its score. A prefix that was guessed and not reported is how a mostly-empty
//! match gets mistaken for a design that mostly has no signals.
//!
//! Two kinds of net never match, and both are reported rather than counted as
//! failures. A net RTLScope invented — an inlined function's local, the wire
//! behind an expression in a port connection — has no counterpart in the
//! source and so none in the dump. And a name that matches at a different
//! width is not the same wire; saying so beats binding it and reading the
//! wrong bits.

use std::collections::{BTreeMap, HashMap};

use rtlscope_analyse::flat::{Flattened, SignalId};
use rtlscope_ir::Design;
use serde::Serialize;

use crate::dump::{Dump, WaveVar};

#[derive(Debug, Clone, Serialize)]
pub struct MatchReport {
    /// The scope the design was found under, `""` if the dump starts at the
    /// top module itself.
    pub prefix: String,
    /// How many nets that prefix matched, against how many it was tried on —
    /// the evidence for the choice.
    pub prefix_score: (usize, usize),
    pub matched: Vec<Matched>,
    /// Nets the design has that the dump does not, excluding ones RTLScope
    /// invented, each with the reason.
    pub unmatched_ir: Vec<Unmatched>,
    /// Variables the dump has that the design does not — the testbench's own
    /// signals, mostly.
    pub unmatched_dump: Vec<String>,
    /// Nets left out of the counting because they exist only in the IR.
    pub synthesised: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Matched {
    pub signal: SignalId,
    /// The name in the design, without the dump's prefix.
    pub ir_name: String,
    pub dump_path: String,
    pub width: u32,
    #[serde(skip)]
    pub var: WaveVar,
}

#[derive(Debug, Clone, Serialize)]
pub struct Unmatched {
    pub ir_name: String,
    pub reason: String,
}

impl MatchReport {
    /// The report for a dump opened with no design to read it against.
    ///
    /// Not a match that failed — there was nothing to match. Every count is
    /// zero and every list empty, so a panel holding this falls back to the
    /// dump's own names, which is all a recording opened on its own has.
    pub fn unmatched() -> Self {
        MatchReport {
            prefix: String::new(),
            prefix_score: (0, 0),
            matched: Vec::new(),
            unmatched_ir: Vec::new(),
            unmatched_dump: Vec::new(),
            synthesised: 0,
        }
    }

    pub fn matched_count(&self) -> usize {
        self.matched.len()
    }

    /// The dump variable for a design signal, if the two were matched.
    pub fn var_of(&self, signal: SignalId) -> Option<WaveVar> {
        self.matched.iter().find(|m| m.signal == signal).map(|m| m.var)
    }

    /// The match for a variable, by the name the *dump* knows it by.
    ///
    /// The mirror of [`MatchReport::by_ir_name`], and needed wherever the dump
    /// asks the question rather than the design: a hierarchy built out of the
    /// dump's own paths has to say, of each leaf, whether the design has it.
    pub fn by_dump_path(&self, path: &str) -> Option<&Matched> {
        self.matched.iter().find(|m| m.dump_path == path)
    }

    pub fn by_ir_name(&self, name: &str) -> Option<&Matched> {
        self.matched.iter().find(|m| m.ir_name == name)
    }
}

/// Matches a dump against a design.
///
/// `prefix` names the scope the design sits under in the dump; `None` infers
/// it.
pub fn match_signals(
    dump: &Dump,
    design: &Design,
    flat: &Flattened,
    prefix: Option<&str>,
) -> MatchReport {
    // One entry per name the design knows a signal by. A signal reached
    // through several instances has several, and the dump may record any of
    // them.
    let mut ir_names: BTreeMap<String, (SignalId, u32)> = BTreeMap::new();
    let mut synthesised = 0usize;
    for (name, signal, width, invented) in flat.all_names(design) {
        if invented {
            synthesised += 1;
            continue;
        }
        ir_names.insert(name, (signal, width));
    }

    let dump_vars: Vec<(&str, WaveVar)> = dump.vars().collect();
    let (prefix, prefix_score) = match prefix {
        Some(given) => {
            let hits = count_matches(&dump_vars, &ir_names, given);
            (given.to_string(), (hits, ir_names.len()))
        }
        None => infer_prefix(&dump_vars, &ir_names),
    };

    let mut matched = Vec::new();
    let mut used: HashMap<&str, ()> = HashMap::new();
    let mut unmatched_ir = Vec::new();

    for (ir_name, (signal, width)) in &ir_names {
        let wanted = join(&prefix, ir_name);
        let Some((path, var)) = dump_vars.iter().find(|(path, _)| *path == wanted) else {
            unmatched_ir.push(Unmatched {
                ir_name: ir_name.clone(),
                reason: format!("no `{wanted}` in the dump"),
            });
            continue;
        };
        // A name that matches at another width is a different wire wearing the
        // same name, and binding it would read the wrong bits.
        match dump.width(*var) {
            Some(found) if found != *width => {
                unmatched_ir.push(Unmatched {
                    ir_name: ir_name.clone(),
                    reason: format!("`{wanted}` is {found} bits, the design says {width}"),
                });
                continue;
            }
            None => {
                unmatched_ir.push(Unmatched {
                    ir_name: ir_name.clone(),
                    reason: format!("`{wanted}` is a real or string variable"),
                });
                continue;
            }
            Some(_) => {}
        }

        used.insert(*path, ());
        matched.push(Matched {
            signal: *signal,
            ir_name: ir_name.clone(),
            dump_path: (*path).to_string(),
            width: *width,
            var: *var,
        });
    }

    let unmatched_dump = dump_vars
        .iter()
        .filter(|(path, _)| !used.contains_key(path))
        .map(|(path, _)| (*path).to_string())
        .collect();

    MatchReport { prefix, prefix_score, matched, unmatched_ir, unmatched_dump, synthesised }
}

/// Tries every scope in the dump as a prefix and keeps the best.
fn infer_prefix(
    dump_vars: &[(&str, WaveVar)],
    ir_names: &BTreeMap<String, (SignalId, u32)>,
) -> (String, (usize, usize)) {
    // Every scope path a variable sits in, plus the empty one for a dump that
    // starts at the design itself.
    let mut candidates: Vec<String> = vec![String::new()];
    for (path, _) in dump_vars {
        let mut scope = *path;
        while let Some(cut) = scope.rfind('.') {
            scope = &scope[..cut];
            let owned = scope.to_string();
            if !candidates.contains(&owned) {
                candidates.push(owned);
            }
        }
    }

    let mut best = (String::new(), 0usize);
    for candidate in candidates {
        let hits = count_matches(dump_vars, ir_names, &candidate);
        // Ties go to the shallower scope: `tb.dut` and `tb.dut.u_rx` can both
        // match when a module is instantiated once, and the outer one is the
        // design.
        let better = hits > best.1 || (hits == best.1 && candidate.len() < best.0.len());
        if hits > 0 && better {
            best = (candidate, hits);
        }
    }
    let total = ir_names.len();
    (best.0, (best.1, total))
}

fn count_matches(
    dump_vars: &[(&str, WaveVar)],
    ir_names: &BTreeMap<String, (SignalId, u32)>,
    prefix: &str,
) -> usize {
    let paths: std::collections::HashSet<&str> = dump_vars.iter().map(|(p, _)| *p).collect();
    ir_names.keys().filter(|name| paths.contains(join(prefix, name).as_str())).count()
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() { name.to_string() } else { format!("{prefix}.{name}") }
}
