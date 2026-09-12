//! Checking RTLScope's IR against Yosys's netlist.
//!
//! Two front ends read the same files. Where they agree, the fact is worth more
//! than either one's word for it; where they differ, one of them is wrong and
//! it is usually this one. That is the whole value here — not the netlist, but
//! the *second opinion*, which is the only thing that catches a front end whose
//! answers are wrong in a way its own tests cannot see.
//!
//! So this compares what both tools genuinely claim to know: which modules
//! exist, what their ports are called and how wide they are, what each
//! parameter evaluated to, and what instantiates what. Everything else Yosys
//! carries is about Yosys — the cells it built out of an `always` block, the
//! nets it invented to hold them — and comparing against it would report
//! differences that mean nothing.
//!
//! Three things are deliberately *not* counted as disagreements, and each is
//! reported instead:
//!
//! - **A module with no source.** RTLScope draws an IP stub as a black box;
//!   Yosys, given the same files, has nothing to draw at all. Neither is wrong.
//! - **A memory.** RTLScope keeps `mem[0:15]` as a net; Yosys moves it out of
//!   `netnames` entirely. The two are talking about the same thing in different
//!   places.
//! - **A net RTLScope invented.** An inlined function's locals have no
//!   counterpart in anybody else's netlist, by construction.

use std::collections::BTreeMap;

use rtlscope_ir::{Design, NetKind, PortDir};
use serde::Serialize;

use crate::netlist::{self, Netlist};

/// How many of each thing to name before saying how many more there were.
const LISTED: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Module,
    Port,
    Direction,
    Width,
    Parameter,
    Instance,
    Net,
    Top,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Difference {
    pub module: String,
    pub kind: Kind,
    pub name: String,
    /// What RTLScope says, and what the netlist says. Either may be `—` for a
    /// thing one of them does not have at all.
    pub ir: String,
    pub netlist: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModuleCheck {
    /// The name in the source, which both tools agree on.
    pub name: String,
    pub ir_name: String,
    pub netlist_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// How many facts about this module were compared, and how many differed.
    pub checked: usize,
    pub differences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrossCheck {
    /// The version of Yosys that wrote the netlist.
    pub creator: String,
    /// How many facts were compared in all.
    pub checked: usize,
    pub modules: Vec<ModuleCheck>,
    pub differences: Vec<Difference>,
    /// Modules one side has and the other does not, once the explainable ones
    /// have been moved into `notes`.
    pub only_in_ir: Vec<String>,
    pub only_in_netlist: Vec<String>,
    /// What could not be compared, and why. Read this before trusting a clean
    /// result: a check that compared nothing also reports no differences.
    pub notes: Vec<String>,
}

impl CrossCheck {
    pub fn agrees(&self) -> bool {
        self.differences.is_empty() && self.only_in_ir.is_empty() && self.only_in_netlist.is_empty()
    }
}

pub fn check(design: &Design, netlist: &Netlist) -> CrossCheck {
    let mut report = CrossCheck {
        creator: netlist.creator.clone(),
        checked: 0,
        modules: Vec::new(),
        differences: Vec::new(),
        only_in_ir: Vec::new(),
        only_in_netlist: Vec::new(),
        notes: Vec::new(),
    };

    // Both sides specialise a parameterised module into several, so the shared
    // key is the name in the source rather than the name either tool made up.
    let mut ours: BTreeMap<&str, Vec<&rtlscope_ir::Module>> = BTreeMap::new();
    let mut blackboxes = Vec::new();
    for module in design.modules.iter() {
        if module.is_blackbox {
            blackboxes.push(module.base_name.clone());
            continue;
        }
        ours.entry(module.base_name.as_str()).or_default().push(module);
    }
    let mut theirs: BTreeMap<&str, Vec<&netlist::Module>> = BTreeMap::new();
    for module in &netlist.modules {
        theirs.entry(module.base_name.as_str()).or_default().push(module);
    }

    blackboxes.sort();
    blackboxes.dedup();
    if !blackboxes.is_empty() {
        report.notes.push(format!(
            "{} module(s) have no source and are black boxes here, so the netlist has nothing \
             to compare them against: {}",
            blackboxes.len(),
            named(&blackboxes)
        ));
    }

    for name in ours.keys() {
        if !theirs.contains_key(name) {
            report.only_in_ir.push((*name).to_string());
        }
    }
    for name in theirs.keys() {
        if !ours.contains_key(name) {
            report.only_in_netlist.push((*name).to_string());
        }
    }

    // The top is a fact about the design as a whole, and getting it wrong makes
    // everything under it the wrong answer to a different question.
    let our_top = &design.top_module().base_name;
    match netlist.modules.iter().find(|module| module.is_top) {
        Some(their_top) if their_top.base_name != *our_top => {
            report.differences.push(Difference {
                module: our_top.clone(),
                kind: Kind::Top,
                name: "top".to_string(),
                ir: our_top.clone(),
                netlist: their_top.base_name.clone(),
            });
            report.checked += 1;
        }
        Some(_) => report.checked += 1,
        None => report.notes.push(
            "the netlist marks no module as the top, so which one it is was not compared — \
             pass `hierarchy -top <NAME>` to Yosys"
                .to_string(),
        ),
    }

    for (name, mine) in &ours {
        let Some(yours) = theirs.get(name) else { continue };
        let (paired, unpaired) = pair_up(mine, yours);
        if !unpaired.is_empty() {
            report.notes.push(format!(
                "`{name}` is built {} way(s) here and {} in the netlist, and {} of them could \
                 not be told apart by their parameters, so they were not compared: {}",
                mine.len(),
                yours.len(),
                unpaired.len(),
                named(&unpaired)
            ));
        }
        for (mine, yours) in paired {
            let before = report.differences.len();
            let checked = compare_module(design, &mine.name, mine, yours, &mut report);
            report.modules.push(ModuleCheck {
                name: mine.name.clone(),
                ir_name: mine.name.clone(),
                netlist_name: yours.name.clone(),
                location: yours.location.clone(),
                checked,
                differences: report.differences.len() - before,
            });
            report.checked += checked;
        }
    }

    report
}

/// Matches each of a module's specialisations to the netlist's, by what the
/// parameters came out as.
///
/// Pairing them by the order they happen to be stored in is the one thing that
/// must not be done: a design with `W=8` and `W=16` would then be compared
/// against itself the wrong way round and report every width as a
/// disagreement. A check that cries wolf is worse than no check, because the
/// next real difference is the one nobody reads.
///
/// What comes back is the pairs, and the names of everything that could not be
/// paired — which is reported rather than quietly compared against something.
fn pair_up<'a>(
    mine: &[&'a rtlscope_ir::Module],
    yours: &[&'a netlist::Module],
) -> (Vec<(&'a rtlscope_ir::Module, &'a netlist::Module)>, Vec<String>) {
    if mine.len() == 1 && yours.len() == 1 {
        return (vec![(mine[0], yours[0])], Vec::new());
    }

    let mut taken = vec![false; yours.len()];
    let mut paired = Vec::new();
    let mut unpaired = Vec::new();

    for module in mine {
        let overridable: Vec<&rtlscope_ir::Param> =
            module.params.iter().filter(|param| !param.is_local).collect();
        let found = yours.iter().enumerate().position(|(index, other)| {
            !taken[index]
                && overridable.iter().all(|param| {
                    other
                        .params
                        .iter()
                        .find(|(name, _)| *name == param.name)
                        .is_some_and(|(_, value)| value.is(param.value))
                })
        });
        match found {
            Some(index) => {
                taken[index] = true;
                paired.push((*module, yours[index]));
            }
            None => unpaired.push(module.name.clone()),
        }
    }
    for (index, other) in yours.iter().enumerate() {
        if !taken[index] {
            unpaired.push(other.name.clone());
        }
    }
    (paired, unpaired)
}

fn compare_module(
    design: &Design,
    name: &str,
    mine: &rtlscope_ir::Module,
    yours: &netlist::Module,
    report: &mut CrossCheck,
) -> usize {
    let mut checked = 1; // the module itself being on both sides
    let mut differ = |kind: Kind, what: &str, ir: String, netlist: String| {
        report.differences.push(Difference {
            module: name.to_string(),
            kind,
            name: what.to_string(),
            ir,
            netlist,
        });
    };

    // ---- ports: the interface, which is what a second opinion is worth most on
    let theirs: BTreeMap<&str, &netlist::Port> =
        yours.ports.iter().map(|port| (port.name.as_str(), port)).collect();
    let mut seen = std::collections::BTreeSet::new();

    for port in &mine.ports {
        checked += 1;
        seen.insert(port.name.as_str());
        let Some(other) = theirs.get(port.name.as_str()) else {
            differ(Kind::Port, &port.name, direction(port.dir).to_string(), "—".to_string());
            continue;
        };
        if port.dir != other.dir {
            differ(
                Kind::Direction,
                &port.name,
                direction(port.dir).to_string(),
                direction(other.dir).to_string(),
            );
        }
        let width = mine.net(port.net).width;
        if width != other.width {
            differ(
                Kind::Width,
                &port.name,
                format!("{width} bit(s)"),
                format!("{} bit(s)", other.width),
            );
        }
    }
    for port in &yours.ports {
        if !seen.contains(port.name.as_str()) {
            checked += 1;
            differ(Kind::Port, &port.name, "—".to_string(), direction(port.dir).to_string());
        }
    }

    // ---- parameters: where a front end is most easily and most quietly wrong
    let mut locals = 0usize;
    for param in &mine.params {
        if param.is_local {
            locals += 1;
            continue;
        }
        checked += 1;
        match yours.params.iter().find(|(other, _)| *other == param.name) {
            Some((_, value)) if value.is(param.value) => {}
            Some((_, value)) => {
                differ(Kind::Parameter, &param.name, param.value.to_string(), value.shown())
            }
            None => differ(Kind::Parameter, &param.name, param.value.to_string(), "—".to_string()),
        }
    }
    if locals > 0 {
        report.notes.push(format!(
            "`{name}` has {locals} localparam(s); Yosys folds those into constants rather than \
             listing them, so they were not compared"
        ));
    }

    // ---- instances: what the hierarchy actually is
    let theirs: BTreeMap<&str, &netlist::Instance> =
        yours.instances.iter().map(|inst| (inst.name.as_str(), inst)).collect();
    let mut seen = std::collections::BTreeSet::new();
    for inst in &mine.insts {
        checked += 1;
        seen.insert(inst.name.as_str());
        let child = &design.modules[inst.of].base_name;
        match theirs.get(inst.name.as_str()) {
            Some(other) if other.of != *child => {
                differ(Kind::Instance, &inst.name, child.clone(), other.of.clone())
            }
            Some(_) => {}
            None => differ(Kind::Instance, &inst.name, child.clone(), "—".to_string()),
        }
    }
    for inst in &yours.instances {
        if !seen.contains(inst.name.as_str()) {
            checked += 1;
            differ(Kind::Instance, &inst.name, "—".to_string(), inst.of.clone());
        }
    }

    // ---- nets: widths only, and only for the ones both tools name
    let theirs: BTreeMap<&str, &netlist::Net> =
        yours.nets.iter().map(|net| (net.name.as_str(), net)).collect();
    let (mut invented, mut memories, mut unmatched) = (0usize, 0usize, Vec::new());
    for net in mine.nets.iter() {
        if net.synthesised {
            invented += 1;
            continue;
        }
        if matches!(net.kind, NetKind::Memory { .. }) {
            memories += 1;
            continue;
        }
        match theirs.get(net.name.as_str()) {
            Some(other) => {
                checked += 1;
                if other.width != net.width {
                    differ(
                        Kind::Net,
                        &net.name,
                        format!("{} bit(s)", net.width),
                        format!("{} bit(s)", other.width),
                    );
                }
            }
            // Yosys drops a net that nothing reads, and renames one it merged
            // with another. Neither is RTLScope being wrong about the source, so
            // it is counted and named rather than reported as a difference.
            None => unmatched.push(net.name.clone()),
        }
    }
    if invented > 0 {
        report.notes.push(format!(
            "`{name}`: {invented} net(s) exist only in the IR — an inlined function's locals, \
             the wire behind an expression in a connection — and have no counterpart in any \
             netlist"
        ));
    }
    if memories > 0 {
        report.notes.push(format!(
            "`{name}`: {memories} memor(y/ies) are nets here and live outside `netnames` in the \
             netlist, so they were not compared"
        ));
    }
    if !unmatched.is_empty() {
        report.notes.push(format!(
            "`{name}`: {} net(s) the netlist does not name, which is what Yosys does to a net \
             nothing reads or one it merged away: {}",
            unmatched.len(),
            named(&unmatched)
        ));
    }

    checked
}

fn direction(dir: PortDir) -> &'static str {
    match dir {
        PortDir::Input => "input",
        PortDir::Output => "output",
        PortDir::Inout => "inout",
    }
}

fn named(items: &[String]) -> String {
    if items.len() <= LISTED {
        return items.join(", ");
    }
    format!("{}, and {} more", items[..LISTED].join(", "), items.len() - LISTED)
}
