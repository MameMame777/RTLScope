//! Guessing which signals make up a bus.
//!
//! Naming a decoder's six or seven channels by hand is tedious and, on a design
//! with two dozen streams, the part most likely to go wrong. But the names are
//! not arbitrary: a stream is called `in_valid`/`in_pixel`/`in_sof` or
//! `m_axis_tvalid`/`m_axis_tready`, and the group prefix is what ties them
//! together.
//!
//! So this proposes bindings by looking for those shapes. It is a suggestion
//! and says so: what it found, what it could not, and enough for a person to
//! see whether it guessed right. A hand-written `--map` always wins.
//!
//! The patterns are the ones measured in the design this was built against
//! rather than the ones a specification implies — `_pixel` as well as `_data`,
//! `_drive_low` for an open-drain bus.

use std::collections::BTreeMap;

use rtlscope_ir::{Design, ModuleId, ProcKind};
use serde::Serialize;

use crate::decode::Binding;

/// A bus this found, and the bindings it proposes for it.
#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    pub protocol: String,
    /// The prefix the signals share, which is what names the bus:
    /// `m_axis`, `in`, `m_byte`, or `sccb` for a two-wire one.
    pub group: String,
    /// Ready to pass to `decode`, once a dump prefix is put in front.
    #[serde(serialize_with = "as_text")]
    pub bindings: Vec<Binding>,
    /// Channels the decoder wants that no signal was found for.
    pub missing: Vec<&'static str>,
}

impl Suggestion {
    /// The bindings as `--map` arguments, with a dump prefix in front of each.
    pub fn mapped(&self, prefix: &str) -> Vec<Binding> {
        self.bindings
            .iter()
            .map(|binding| Binding {
                role: binding.role.clone(),
                path: if prefix.is_empty() {
                    binding.path.clone()
                } else {
                    format!("{prefix}.{}", binding.path)
                },
                invert: binding.invert,
            })
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

fn as_text<S: serde::Serializer>(bindings: &[Binding], out: S) -> Result<S::Ok, S::Error> {
    let text: Vec<String> = bindings
        .iter()
        .map(|b| format!("{}={}{}", b.role, if b.invert { "!" } else { "" }, b.path))
        .collect();
    out.collect_seq(text)
}

/// What every bus this recognises looks like.
///
/// The first entry of each is the one that identifies the group: find it, and
/// its prefix names the bus.
struct Shape {
    protocol: &'static str,
    /// `(role, suffix, required)` — the suffix after the group's prefix.
    channels: &'static [(&'static str, &'static str, bool)],
}

const SHAPES: &[Shape] = &[
    // First, because its marker is the most specific: a `_awvalid` names a
    // memory-mapped port and nothing else does. All ten handshake signals are
    // required — a port with only one direction is legal AXI4, but a
    // suggestion is a claim that this is a whole bus, and `--map` by hand
    // still decodes half of one.
    Shape {
        protocol: "axi4",
        channels: &[
            ("awvalid", "_awvalid", true),
            ("awready", "_awready", true),
            ("awaddr", "_awaddr", false),
            ("awlen", "_awlen", false),
            ("awsize", "_awsize", false),
            ("awburst", "_awburst", false),
            ("wvalid", "_wvalid", true),
            ("wready", "_wready", true),
            ("wdata", "_wdata", false),
            ("wstrb", "_wstrb", false),
            ("wlast", "_wlast", false),
            ("bvalid", "_bvalid", true),
            ("bready", "_bready", true),
            ("bresp", "_bresp", false),
            ("arvalid", "_arvalid", true),
            ("arready", "_arready", true),
            ("araddr", "_araddr", false),
            ("arlen", "_arlen", false),
            ("arsize", "_arsize", false),
            ("arburst", "_arburst", false),
            ("rvalid", "_rvalid", true),
            ("rready", "_rready", true),
            ("rdata", "_rdata", false),
            ("rresp", "_rresp", false),
            ("rlast", "_rlast", false),
        ],
    },
    Shape {
        protocol: "axis",
        channels: &[
            ("tvalid", "_tvalid", true),
            ("tready", "_tready", true),
            ("tdata", "_tdata", false),
            ("tlast", "_tlast", false),
            ("tuser", "_tuser", false),
            ("tkeep", "_tkeep", false),
        ],
    },
    Shape {
        protocol: "pixel",
        channels: &[
            ("valid", "_valid", true),
            ("data", "_pixel", false),
            ("sof", "_sof", false),
            ("eol", "_eol", false),
            ("eof", "_eof", false),
            ("err", "_err", false),
        ],
    },
];

/// Everything that looks like a bus on this module.
///
/// `instance_path` is where the module sits in the design, and is put in front
/// of every name so the result reads against a flattened dump.
pub fn suggest(design: &Design, module_id: ModuleId, instance_path: &str) -> Vec<Suggestion> {
    let module = &design.modules[module_id];

    // Nets rather than ports: a stream worth decoding is as often internal as
    // it is on the boundary. Nets RTLScope invented have no counterpart in a dump.
    let names: Vec<&str> =
        module.nets.iter().filter(|net| !net.synthesised).map(|net| net.name.as_str()).collect();
    let has = |name: &str| names.contains(&name);

    let clock = clock_of(design, module_id);
    let qualify = |name: &str| {
        if instance_path.is_empty() { name.to_string() } else { format!("{instance_path}.{name}") }
    };

    let mut found: Vec<Suggestion> = Vec::new();
    let mut claimed: BTreeMap<String, ()> = BTreeMap::new();

    for shape in SHAPES {
        let (marker_role, marker_suffix, _) = shape.channels[0];
        for name in &names {
            let Some(group) = name.strip_suffix(marker_suffix) else { continue };
            if group.is_empty() {
                continue;
            }
            // A name can end in more than one shape's marker; the first shape
            // to claim it keeps it, along with the signals it took.
            if claimed.contains_key(*name) {
                continue;
            }

            let mut bindings = Vec::new();
            let mut missing = Vec::new();
            if let Some(clock) = &clock {
                bindings.push(Binding {
                    role: "clock".into(),
                    path: qualify(clock),
                    invert: false,
                });
            } else {
                missing.push("clock");
            }

            let mut taken = vec![(*name).to_string()];
            for (role, suffix, required) in shape.channels {
                let candidate = format!("{group}{suffix}");
                if has(&candidate) {
                    taken.push(candidate.clone());
                    bindings.push(Binding {
                        role: (*role).to_string(),
                        path: qualify(&candidate),
                        invert: false,
                    });
                } else if *role == "data" && shape.protocol == "pixel" {
                    // A pixel stream carries `_pixel` or `_data`, depending on
                    // whether it is thought of as an image or as a bus.
                    let alternative = format!("{group}_data");
                    if has(&alternative) {
                        taken.push(alternative.clone());
                        bindings.push(Binding {
                            role: "data".into(),
                            path: qualify(&alternative),
                            invert: false,
                        });
                    }
                } else if *required {
                    missing.push(role);
                }
            }

            // A `_valid` on its own is not a stream; it is a flag.
            let optional_found = bindings.len() > 2;
            if missing.iter().any(|role| *role != "clock") || !optional_found {
                continue;
            }
            for name in taken {
                claimed.insert(name, ());
            }
            let _ = marker_role;
            found.push(Suggestion {
                protocol: shape.protocol.to_string(),
                group: group.to_string(),
                bindings,
                missing,
            });
        }
    }

    if let Some(i2c) = two_wire_bus(&names, &qualify) {
        found.push(i2c);
    }

    found.sort_by(|a, b| a.protocol.cmp(&b.protocol).then_with(|| a.group.cmp(&b.group)));
    found
}

/// An I²C-shaped pair, however the design chose to model the two wires.
///
/// Preference order matters: a pad readback says what the wire did, including
/// what the *other* end drove, while a drive enable only says what this end
/// asked for — and reads inverted.
fn two_wire_bus(names: &[&str], qualify: &dyn Fn(&str) -> String) -> Option<Suggestion> {
    let find = |line: &str| -> Option<(String, bool)> {
        // A readback of the pad, uninverted, is the best thing to watch.
        let readback = names.iter().find(|name| {
            let name = name.to_ascii_lowercase();
            name.contains(line) && (name.ends_with("_in") || name.starts_with("cam_"))
        });
        if let Some(name) = readback {
            return Some(((*name).to_string(), false));
        }
        // Failing that, the drive enable, which is the opposite of the wire.
        let drive = names
            .iter()
            .find(|name| name.to_ascii_lowercase().contains(line) && name.contains("drive_low"));
        if let Some(name) = drive {
            return Some(((*name).to_string(), true));
        }
        // Or a plainly named wire.
        names
            .iter()
            .find(|name| name.eq_ignore_ascii_case(line))
            .map(|name| ((*name).to_string(), false))
    };

    let (scl, scl_invert) = find("scl")?;
    let (sda, sda_invert) = find("sda")?;
    Some(Suggestion {
        protocol: "i2c".into(),
        group: "sccb".into(),
        bindings: vec![
            Binding { role: "scl".into(), path: qualify(&scl), invert: scl_invert },
            Binding { role: "sda".into(), path: qualify(&sda), invert: sda_invert },
        ],
        missing: Vec::new(),
    })
}

/// The clock this module's registers run on, when they agree on one.
///
/// A module that is only wiring — a top that instantiates a master and a
/// slave and joins them — has no registers of its own, so the question is put
/// to its children instead: whatever clock they run on, followed back out
/// through the port it arrives by, is this module's clock too. That is the
/// module a reader presses `decode…` on when they are looking at a bus, and a
/// suggestion missing its clock is one they cannot run.
fn clock_of(design: &Design, module_id: ModuleId) -> Option<String> {
    let module = &design.modules[module_id];
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for process in &module.procs {
        let ProcKind::Ff { clk, .. } = &process.kind else { continue };
        if let Some(net) = clk.net_id() {
            *counts.entry(module.net(net).name.clone()).or_default() += 1;
        }
    }
    if counts.is_empty() {
        for instance in &module.insts {
            let child = &design.modules[instance.of];
            let Some(inner) = clock_of(design, instance.of) else { continue };
            // The child's clock is one of its ports, or it is internal and
            // says nothing about this module.
            let Some(port_name) = child
                .ports
                .iter()
                .find(|port| child.net(port.net).name == inner)
                .map(|port| port.name.as_str())
            else {
                continue;
            };
            let bound = instance
                .conns
                .iter()
                .find(|conn| child.port(conn.port).name == port_name)
                .and_then(|conn| conn.net.net_id());
            if let Some(net) = bound {
                *counts.entry(module.net(net).name.clone()).or_default() += 1;
            }
        }
    }
    // The busiest, since a module with two clocks has a main one.
    counts.into_iter().max_by_key(|(_, count)| *count).map(|(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtlscope_sv::ParseOptions;

    fn suggestions(fixture: &str, top: Option<&str>) -> Vec<Suggestion> {
        let path = rtlscope_fixtures::path(fixture);
        let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
        let (design, _) = rtlscope_elab::elaborate(&uir, top);
        let design = design.expect("elaboration produced a design");
        suggest(&design, design.top, "")
    }

    #[test]
    fn a_module_with_no_bus_suggests_nothing() {
        let found = suggestions("counter.sv", None);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_clock_comes_from_what_it_clocks() {
        let path = rtlscope_fixtures::path("counter.sv");
        let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
        let (design, _) = rtlscope_elab::elaborate(&uir, None);
        let design = design.expect("a design");
        assert_eq!(clock_of(&design, design.top).as_deref(), Some("clk"));
    }

    #[test]
    fn an_instance_path_is_put_in_front_of_every_name() {
        let suggestion = Suggestion {
            protocol: "pixel".into(),
            group: "in".into(),
            bindings: vec![Binding {
                role: "valid".into(),
                path: "in_valid".into(),
                invert: false,
            }],
            missing: Vec::new(),
        };
        let mapped = suggestion.mapped("tb.dut");
        assert_eq!(mapped[0].path, "tb.dut.in_valid");
        assert_eq!(suggestion.mapped("")[0].path, "in_valid");
    }
}
