//! `rtlscope dump-ir` — the elaborated design.
//!
//! Unlike `dump-ports`, this runs after elaboration, so every width is a number
//! and every connection names a net. Two shapes are available: the raw IR, which
//! is the JSON contract the Phase 6 MCP server will serve, and a readable
//! summary for looking at by eye.

use rtlscope_ir::{Design, NetKind, NetRef};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct IrReport {
    pub top: String,
    pub modules: Vec<ModuleSummary>,
}

#[derive(Debug, Serialize)]
pub struct ModuleSummary {
    /// Specialised name, e.g. `params_sub$W=16`.
    pub name: String,
    /// Name as written in the source, shared by every specialisation.
    pub base_name: String,
    pub location: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub blackbox: bool,
    pub params: Vec<String>,
    pub ports: Vec<String>,
    pub nets: Vec<String>,
    pub instances: Vec<InstanceSummary>,
    pub processes: Vec<ProcessSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ProcessSummary {
    /// `ff posedge clk, async reset rst_n low` / `comb`.
    pub kind: String,
    pub reads: Vec<String>,
    pub writes: Vec<String>,
    pub location: String,
}

#[derive(Debug, Serialize)]
pub struct InstanceSummary {
    pub name: String,
    pub of: String,
    pub connections: Vec<String>,
}

pub fn build(design: &Design) -> IrReport {
    IrReport {
        top: design.top_module().name.clone(),
        modules: design
            .modules
            .iter()
            .map(|module| ModuleSummary {
                name: module.name.clone(),
                base_name: module.base_name.clone(),
                location: design.files.render(module.span),
                blackbox: module.is_blackbox,
                params: module
                    .params
                    .iter()
                    .map(|p| {
                        let keyword = if p.is_local { "localparam" } else { "parameter" };
                        format!("{keyword} {} = {}", p.name, p.value)
                    })
                    .collect(),
                ports: module
                    .ports
                    .iter()
                    .map(|port| {
                        let net = module.net(port.net);
                        format!(
                            "{:<6} {}",
                            format!("{:?}", port.dir).to_lowercase(),
                            describe_net(net)
                        )
                    })
                    .collect(),
                nets: module.nets.iter().map(describe_net).collect(),
                instances: module
                    .insts
                    .iter()
                    .map(|inst| InstanceSummary {
                        name: inst.name.clone(),
                        of: design.modules[inst.of].name.clone(),
                        connections: inst
                            .conns
                            .iter()
                            .map(|conn| {
                                let child = &design.modules[inst.of];
                                let port_name = &child.ports[conn.port.0 as usize].name;
                                format!(".{port_name}({})", describe_ref(module, &conn.net))
                            })
                            .collect(),
                    })
                    .collect(),
                processes: module
                    .procs
                    .iter()
                    .map(|process| ProcessSummary {
                        kind: describe_kind(module, &process.kind),
                        reads: process.reads.iter().map(|r| describe_ref(module, r)).collect(),
                        writes: process.writes.iter().map(|r| describe_ref(module, r)).collect(),
                        location: design.files.render(process.span),
                    })
                    .collect(),
                skipped: module
                    .skipped
                    .iter()
                    .map(|s| format!("{} at {}", s.construct, design.files.render(s.span)))
                    .collect(),
            })
            .collect(),
    }
}

fn describe_kind(module: &rtlscope_ir::Module, kind: &rtlscope_ir::ProcKind) -> String {
    match kind {
        rtlscope_ir::ProcKind::Comb => "comb".to_string(),
        rtlscope_ir::ProcKind::Initial => "initial".to_string(),
        rtlscope_ir::ProcKind::Latch => "latch".to_string(),
        rtlscope_ir::ProcKind::Ff { clk, edge, rst } => {
            let edge = match edge {
                rtlscope_ir::Edge::Pos => "posedge",
                rtlscope_ir::Edge::Neg => "negedge",
            };
            let mut text = format!("ff {edge} {}", describe_ref(module, clk));
            if let Some(reset) = rst {
                let kind = match reset.kind {
                    rtlscope_ir::ResetKind::Async => "async",
                    rtlscope_ir::ResetKind::Sync => "sync",
                };
                let active = match reset.active {
                    rtlscope_ir::Level::High => "high",
                    rtlscope_ir::Level::Low => "low",
                };
                text.push_str(&format!(
                    ", {kind} reset {} active {active}",
                    describe_ref(module, &reset.net)
                ));
            }
            text
        }
    }
}

/// `logic [7:0] data` / `logic [7:0] mem [16]` — width as a number, at last.
fn describe_net(net: &rtlscope_ir::Net) -> String {
    let vector = if net.width == 1 { String::new() } else { format!("[{}:0] ", net.width - 1) };
    match net.kind {
        NetKind::Logic => format!("{vector}{}", net.name),
        NetKind::Memory { depth } => format!("{vector}{} [{depth}]", net.name),
    }
}

fn describe_ref(module: &rtlscope_ir::Module, net_ref: &NetRef) -> String {
    match net_ref {
        NetRef::Full { net } => module.net(*net).name.clone(),
        NetRef::Slice { net, msb, lsb } => {
            let name = &module.net(*net).name;
            if msb == lsb { format!("{name}[{msb}]") } else { format!("{name}[{msb}:{lsb}]") }
        }
        NetRef::Const { value } => match value.to_u64() {
            Some(v) => format!("{}'d{v}", value.width),
            None => format!("{}'h<wide>", value.width),
        },
    }
}
