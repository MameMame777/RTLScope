//! The design with its hierarchy resolved away.
//!
//! A net called `byte_clk` inside one module and `clk` inside another may be
//! the same wire, and two modules that both call their clock `clk` may not be.
//! Any question about the design as a whole — which clock a register really
//! runs on, which register feeds which — has to be asked of wires rather than
//! of names.
//!
//! So the instance tree is walked from the top and every module-local net is
//! given the identity of the parent net it is connected to. What comes back is
//! one [`SignalId`] per wire and one [`Node`] per *instance* of a module, since
//! two instances of the same module are two different pieces of hardware even
//! though they share a [`Module`].

use std::collections::{BTreeMap, HashMap};

use rtlscope_ir::{Conn, Design, Module, ModuleId, NetId, NetRef};

/// One wire, however many module-local names it has.
pub type SignalId = u32;

/// One instance of one module, with its nets resolved to design-wide signals.
pub struct Node {
    pub module: ModuleId,
    /// `""` for the top, `u_rx.u_align` for something nested.
    pub path: String,
    pub signals: HashMap<NetId, SignalId>,
}

pub struct Flattened {
    pub nodes: Vec<Node>,
    /// The shallowest name seen for each signal, which is the one a reader will
    /// recognise: `pix_clk` rather than `u_hdmi.u_serdes.clk`.
    names: BTreeMap<SignalId, String>,
    depths: BTreeMap<SignalId, usize>,
    /// Where each signal lives: every `(node, net)` that is this one wire.
    ///
    /// The walk builds the forward map — a net's signal — because that is what
    /// it needs to bind ports. Going back the other way is the question a
    /// waveform asks: this track is signal 41, so which net of which module do
    /// I show? Without an index every such question is a scan of the design.
    homes: BTreeMap<SignalId, Vec<(usize, NetId)>>,
    next: SignalId,
}

/// How deep the instance tree may go before this gives up.
///
/// Elaboration has already rejected genuine recursion; this is only here so a
/// bug cannot turn into a hang.
const MAX_DEPTH: usize = 64;

pub fn flatten(design: &Design) -> Flattened {
    let mut flat = Flattened {
        nodes: Vec::new(),
        names: BTreeMap::new(),
        depths: BTreeMap::new(),
        homes: BTreeMap::new(),
        next: 0,
    };
    flat.visit(design, design.top, String::new(), &HashMap::new(), 0);
    flat.index_homes();
    flat
}

impl Flattened {
    fn visit(
        &mut self,
        design: &Design,
        module_id: ModuleId,
        path: String,
        bound: &HashMap<NetId, SignalId>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let module = &design.modules[module_id];

        // A net connected to a port *is* the parent's net; everything else is
        // new here.
        let mut signals = HashMap::with_capacity(module.nets.len());
        for net in module.nets.indices() {
            let signal = match bound.get(&net) {
                Some(signal) => *signal,
                None => {
                    self.next += 1;
                    self.next - 1
                }
            };
            signals.insert(net, signal);

            let name = if path.is_empty() {
                module.net(net).name.clone()
            } else {
                format!("{path}.{}", module.net(net).name)
            };
            if self.depths.get(&signal).is_none_or(|seen| depth < *seen) {
                self.depths.insert(signal, depth);
                self.names.insert(signal, name);
            }
        }

        for instance in &module.insts {
            let child = &design.modules[instance.of];
            let mut child_bound = HashMap::new();
            for Conn { port, net: net_ref, .. } in &instance.conns {
                let Some(parent) = net_ref.net_id().and_then(|net| signals.get(&net).copied())
                else {
                    continue;
                };
                let Some(port) = child.ports.get(port.0 as usize) else { continue };
                child_bound.insert(port.net, parent);
            }
            let child_path = if path.is_empty() {
                instance.name.clone()
            } else {
                format!("{path}.{}", instance.name)
            };
            self.visit(design, instance.of, child_path, &child_bound, depth + 1);
        }

        self.nodes.push(Node { module: module_id, path, signals });
    }

    /// Builds the reverse index, once the walk has settled.
    ///
    /// After rather than during: a node's index is only decided when it is
    /// pushed, and that happens after its children have been. Sorted, because
    /// the forward maps are hashed and a report that lists the same homes in a
    /// different order each run is a report nothing can be tested against.
    fn index_homes(&mut self) {
        for (index, node) in self.nodes.iter().enumerate() {
            for (net, signal) in &node.signals {
                self.homes.entry(*signal).or_default().push((index, *net));
            }
        }
        for homes in self.homes.values_mut() {
            homes.sort_unstable();
        }
    }

    /// Every net that is this one wire, as `(node, net)`.
    ///
    /// More than one, usually: a signal crossing three module boundaries has a
    /// name in each. They come shallowest node first only by accident of the
    /// walk order, so a caller that wants the outermost should say so rather
    /// than take the first.
    pub fn homes(&self, signal: SignalId) -> &[(usize, NetId)] {
        self.homes.get(&signal).map_or(&[], Vec::as_slice)
    }

    /// Every name every signal goes by, with the width and provenance of the
    /// net it came from.
    ///
    /// [`Flattened::name_of`] gives one name per signal — the shallowest, which
    /// reads best — but matching against a dump needs the opposite: a dump
    /// records whatever name the scope it sits in gave it, so every alias has
    /// to be offered.
    pub fn all_names<'a>(
        &'a self,
        design: &'a Design,
    ) -> impl Iterator<Item = (String, SignalId, u32, bool)> + 'a {
        self.nodes.iter().flat_map(move |node| {
            let module = &design.modules[node.module];
            node.signals.iter().filter_map(move |(net, signal)| {
                let net = module.nets.get(*net)?;
                let name = if node.path.is_empty() {
                    net.name.clone()
                } else {
                    format!("{}.{}", node.path, net.name)
                };
                Some((name, *signal, net.width, net.synthesised))
            })
        })
    }

    pub fn name_of(&self, signal: SignalId) -> String {
        self.names.get(&signal).cloned().unwrap_or_else(|| format!("signal#{signal}"))
    }

    /// How many distinct wires the design has.
    pub fn signals(&self) -> usize {
        self.next as usize
    }
}

impl Node {
    /// The signal a net in this instance belongs to.
    pub fn signal(&self, net: NetId) -> Option<SignalId> {
        self.signals.get(&net).copied()
    }

    /// The signal a reference points at, if it points at a net at all.
    pub fn signal_of(&self, net_ref: &NetRef) -> Option<SignalId> {
        net_ref.net_id().and_then(|net| self.signal(net))
    }

    pub fn module<'a>(&self, design: &'a Design) -> &'a Module {
        &design.modules[self.module]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtlscope_sv::ParseOptions;

    fn design(fixture: &str, top: Option<&str>) -> Design {
        let path = rtlscope_fixtures::path(fixture);
        let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
        rtlscope_elab::elaborate(&uir, top).0.expect("elaborates")
    }

    /// The reverse index agreeing with the forward one is the whole of its
    /// correctness, and the design is small enough to check outright rather
    /// than sample.
    #[test]
    fn every_home_leads_back_to_the_signal_it_is_filed_under() {
        let design = design("hier.sv", Some("hier_top"));
        let flat = flatten(&design);

        let mut counted = 0;
        for signal in 0..flat.signals() as SignalId {
            let homes = flat.homes(signal);
            assert!(homes.is_sorted(), "signal {signal} is filed out of order");
            for (node, net) in homes {
                assert_eq!(flat.nodes[*node].signal(*net), Some(signal));
                counted += 1;
            }
        }

        let total: usize = flat.nodes.iter().map(|node| node.signals.len()).sum();
        assert_eq!(counted, total, "every net of every node is filed exactly once");
    }

    /// A wire crossing a boundary is one signal wearing a name on each side.
    /// That is what lets a waveform track point at a module: the dump recorded
    /// one of those names, and the reader wants whichever one they are looking
    /// at.
    #[test]
    fn a_net_and_the_port_it_drives_are_one_signal_with_two_homes() {
        let design = design("hier.sv", Some("hier_top"));
        let flat = flatten(&design);
        let top = flat.nodes.iter().find(|node| node.path.is_empty()).expect("the top");
        let (alu_y, _) =
            design.module(top.module).net_by_name("alu_y").expect("hier_top has alu_y");
        let signal = top.signal(alu_y).expect("a net of the top is a signal");

        let named: Vec<String> = flat
            .homes(signal)
            .iter()
            .map(|(node, net)| {
                let node = &flat.nodes[*node];
                let name = &design.module(node.module).net(*net).name;
                match node.path.is_empty() {
                    true => name.clone(),
                    false => format!("{}.{name}", node.path),
                }
            })
            .collect();

        assert!(named.contains(&"alu_y".to_string()), "{named:?}");
        assert!(named.contains(&"u_alu.y".to_string()), "{named:?}");
    }
}
