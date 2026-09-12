//! Finding signals that cross from one clock to another.
//!
//! The IR already says which clock every flop runs on — the front end had to
//! decide that to classify the process at all — so a crossing is a short walk
//! from there: a flop on clock B reading something a flop on clock A wrote.
//! Everything in between is combinational, and combinational logic carries the
//! domain of whatever fed it.
//!
//! The work is in the hierarchy. A clock called `byte_clk` inside one module
//! and `clk` inside another may be the same wire, and two modules that both
//! call their clock `clk` may not be. So the design is flattened first: the
//! instance tree is walked from the top, each module-local net is given the
//! identity of the parent net it is connected to, and domains are compared by
//! that identity rather than by name. Without it, every crossing that happens
//! between two instances — which is most of them — would be invisible.
//!
//! ## What is reported, and what is not
//!
//! A crossing into a two-flop synchroniser is reported as *handled*, not
//! hidden: a report that only lists problems cannot be checked for what it
//! missed. Anything else is reported as unsynchronised, including the async
//! FIFOs and handshakes that are perfectly correct — this analysis recognises
//! one idiom, and says so rather than implying the others are wrong.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rtlscope_ir::{Design, ExprKind, NetId, NetRef, ProcKind, Span, Stmt, StmtKind};
use serde::{Deserialize, Serialize};

use crate::flat::{SignalId, flatten};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainReport {
    pub domains: Vec<Domain>,
    pub crossings: Vec<Crossing>,
    /// How many flops were found in total, as a check on the rest.
    pub flops: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Domain {
    /// The clock's name at the shallowest place it appears.
    pub clock: String,
    pub flops: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crossing {
    /// The signal that crosses, named where it was read.
    pub signal: String,
    pub from: String,
    pub to: String,
    pub kind: CrossingKind,
    /// How many bits cross together.
    pub width: u32,
    /// The instance path of the process that reads it.
    pub at: String,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossingKind {
    /// Straight into a pair of flops on the destination clock with nothing in
    /// between — the one synchroniser this recognises.
    TwoFlopSynchroniser,
    /// One bit, arriving somewhere other than a two-flop synchroniser.
    Unsynchronised,
    /// Several bits at once, with no synchroniser found. Worse than the
    /// single-bit case: the bits settle independently, so the destination can
    /// see a value that was never sent.
    MultiBitUnsynchronised,
}

impl CrossingKind {
    pub fn is_handled(self) -> bool {
        matches!(self, CrossingKind::TwoFlopSynchroniser)
    }
}

pub fn analyse(design: &Design) -> DomainReport {
    let flat = flatten(design);

    // Which domain each flop belongs to, and what each signal is driven by.
    let mut per_domain: BTreeMap<SignalId, usize> = BTreeMap::new();
    let mut driven_by: HashMap<SignalId, BTreeSet<SignalId>> = HashMap::new();
    let mut comb_edges: Vec<(SignalId, Vec<SignalId>)> = Vec::new();
    let mut flops = 0usize;

    for node in &flat.nodes {
        let module = &design.modules[node.module];
        for process in &module.procs {
            let written: Vec<SignalId> = process
                .writes
                .iter()
                .filter_map(|w| w.net_id())
                .filter_map(|net| node.signals.get(&net).copied())
                .collect();
            let read: Vec<SignalId> = process
                .reads
                .iter()
                .filter_map(|r| r.net_id())
                .filter_map(|net| node.signals.get(&net).copied())
                .collect();

            match &process.kind {
                ProcKind::Ff { clk, .. } => {
                    let Some(clock) = clk.net_id().and_then(|n| node.signals.get(&n).copied())
                    else {
                        continue;
                    };
                    flops += 1;
                    *per_domain.entry(clock).or_default() += 1;
                    for signal in written {
                        driven_by.entry(signal).or_default().insert(clock);
                    }
                }
                // Combinational logic has no clock of its own; it passes on
                // whatever drove its inputs.
                ProcKind::Comb | ProcKind::Latch => {
                    for signal in written {
                        comb_edges.push((signal, read.clone()));
                    }
                }
                // Power-on values belong to no clock and cross nothing.
                ProcKind::Initial => {}
            }
        }
    }

    propagate(&mut driven_by, &comb_edges);

    let mut crossings = Vec::new();
    for node in &flat.nodes {
        let module = &design.modules[node.module];
        for process in &module.procs {
            let ProcKind::Ff { clk, rst, .. } = &process.kind else { continue };
            let Some(destination) = clk.net_id().and_then(|n| node.signals.get(&n).copied()) else {
                continue;
            };
            // The clock and the reset are not data and do not cross.
            let ignore: BTreeSet<Option<NetId>> =
                [clk.net_id(), rst.as_ref().and_then(|r| r.net.net_id())].into_iter().collect();

            for read in &process.reads {
                let Some(net) = read.net_id() else { continue };
                if ignore.contains(&Some(net)) {
                    continue;
                }
                let Some(signal) = node.signals.get(&net).copied() else { continue };
                let Some(sources) = driven_by.get(&signal) else { continue };
                let Some(source) = sources.iter().copied().find(|source| *source != destination)
                else {
                    continue;
                };

                let width = module.net(net).width;
                let kind = if synchronised(module, clk, net) {
                    CrossingKind::TwoFlopSynchroniser
                } else if width > 1 {
                    CrossingKind::MultiBitUnsynchronised
                } else {
                    CrossingKind::Unsynchronised
                };

                crossings.push(Crossing {
                    signal: flat.name_of(signal),
                    from: flat.name_of(source),
                    to: flat.name_of(destination),
                    kind,
                    width,
                    at: if node.path.is_empty() { "(top)".into() } else { node.path.clone() },
                    span: process.span,
                });
            }
        }
    }

    crossings.sort_by(|a, b| {
        (a.kind.is_handled(), &a.from, &a.to, &a.signal).cmp(&(
            b.kind.is_handled(),
            &b.from,
            &b.to,
            &b.signal,
        ))
    });
    crossings.dedup_by(|a, b| a.signal == b.signal && a.from == b.from && a.to == b.to);

    let mut domains: Vec<Domain> = per_domain
        .into_iter()
        .map(|(clock, flops)| Domain { clock: flat.name_of(clock), flops })
        .collect();
    domains.sort_by(|a, b| b.flops.cmp(&a.flops).then_with(|| a.clock.cmp(&b.clock)));

    DomainReport { domains, crossings, flops }
}

/// Pushes domains forward through combinational logic until nothing changes.
///
/// A comb process passes on every domain that reached its inputs, so a chain of
/// gates between two flops does not hide the crossing behind it.
fn propagate(
    driven_by: &mut HashMap<SignalId, BTreeSet<SignalId>>,
    edges: &[(SignalId, Vec<SignalId>)],
) {
    // Bounded because each round can only add domains, and there are finitely
    // many; the count is a guard against a bug, not against the design.
    for _ in 0..edges.len().max(1) {
        let mut changed = false;
        for (target, sources) in edges {
            let mut incoming = BTreeSet::new();
            for source in sources {
                if let Some(domains) = driven_by.get(source) {
                    incoming.extend(domains.iter().copied());
                }
            }
            if incoming.is_empty() {
                continue;
            }
            let existing = driven_by.entry(*target).or_default();
            let before = existing.len();
            existing.extend(incoming);
            changed |= existing.len() != before;
        }
        if !changed {
            break;
        }
    }
}

/// Whether `net` arrives through a pair of flops on the destination clock.
///
/// The shape is exact: somewhere on this clock `a <= net;` with nothing done to
/// the value, and somewhere on the same clock `b <= a;`. Both stages are often
/// written in the *same* `always_ff` as everything else the block does — a
/// synchroniser is three lines inside a hundred-line reset-and-update block —
/// so what matters is that each assignment is a plain copy, not that the
/// process does nothing else.
///
/// Anything else — a gate in the way, a gray-coded FIFO pointer, a handshake —
/// is not recognised. Reporting those as unsynchronised is the honest answer
/// rather than a claim that they are wrong.
fn synchronised(module: &rtlscope_ir::Module, clock: &NetRef, net: NetId) -> bool {
    let first_stage = direct_copies_on(module, clock, net);
    first_stage.iter().any(|first| !direct_copies_on(module, clock, *first).is_empty())
}

/// Every net assigned straight from `source` by a flop on this clock.
fn direct_copies_on(module: &rtlscope_ir::Module, clock: &NetRef, source: NetId) -> Vec<NetId> {
    let mut out = Vec::new();
    for process in &module.procs {
        let ProcKind::Ff { clk, .. } = &process.kind else { continue };
        if clk.net_id() != clock.net_id() {
            continue;
        }
        direct_copies(&process.body, source, &mut out);
    }
    out
}

fn direct_copies(body: &Stmt, source: NetId, out: &mut Vec<NetId>) {
    body.for_each_stmt(&mut |stmt| {
        let StmtKind::Assign { lhs, rhs, .. } = &stmt.kind else { return };
        if let ExprKind::Ref { net: NetRef::Full { net } } = &rhs.kind
            && *net == source
            && let Some(target) = lhs.net_id()
        {
            out.push(target);
        }
    });
}
