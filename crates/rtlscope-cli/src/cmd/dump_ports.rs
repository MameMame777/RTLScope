//! `rtlscope dump-ports` — what the front end understood, before elaboration.
//!
//! Widths and parameter values are still expressions here, printed as text.
//! That is the point of the command: it shows what was *parsed*, so a front-end
//! problem can be told apart from an elaboration problem.

use rtlscope_ir::{UConns, UDesign, UItem, UModule, UParamOverrides};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct PortsReport {
    pub files: Vec<String>,
    pub modules: Vec<ModuleSummary>,
}

#[derive(Debug, Serialize)]
pub struct ModuleSummary {
    pub name: String,
    pub ansi_header: bool,
    pub location: String,
    /// Header parameters first, then any `localparam` declared in the body.
    pub params: Vec<ParamSummary>,
    pub ports: Vec<PortSummary>,
    /// Nets and variables declared in the body.
    pub nets: Vec<NetSummary>,
    pub instances: Vec<InstanceSummary>,
    /// Constructs the front end skipped, so an empty-looking module is never a
    /// silent one.
    pub skipped: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ParamSummary {
    pub name: String,
    pub local: bool,
    pub default: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PortSummary {
    pub name: String,
    /// `None` on a non-ANSI header until elaboration merges in the body
    /// declaration that states the direction.
    pub dir: Option<String>,
    pub net_type: Option<String>,
    /// The vector dimension as written, e.g. `[(W - 1):0]`.
    pub packed: Option<String>,
    pub unpacked: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NetSummary {
    pub name: String,
    pub net_type: String,
    pub packed: Option<String>,
    /// Present when the declaration has an array dimension, i.e. it is a memory.
    pub unpacked: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InstanceSummary {
    pub module: String,
    pub name: String,
    /// `named`, `positional`, `wildcard` or `none` — the style the author used.
    pub connection_style: &'static str,
    pub connection_count: usize,
    pub param_overrides: Vec<String>,
}

pub fn build(design: &UDesign) -> PortsReport {
    PortsReport {
        files: design.files.iter().map(|(_, path)| path.display().to_string()).collect(),
        modules: design.modules.iter().map(|m| summarise(design, m)).collect(),
    }
}

fn summarise(design: &UDesign, module: &UModule) -> ModuleSummary {
    let summarise_param = |p: &rtlscope_ir::UParam| ParamSummary {
        name: p.name.clone(),
        local: p.is_local,
        default: p.default.as_ref().map(ToString::to_string),
    };
    let mut params: Vec<ParamSummary> = module.params.iter().map(summarise_param).collect();

    let mut ports: Vec<PortSummary> = module
        .ports
        .iter()
        .map(|p| PortSummary {
            name: p.name.clone(),
            dir: p.dir.map(|d| format!("{d:?}").to_lowercase()),
            net_type: p.net_type.map(|t| format!("{t:?}").to_lowercase()),
            packed: p.packed.as_ref().map(ToString::to_string),
            unpacked: p.unpacked.as_ref().map(ToString::to_string),
        })
        .collect();

    let mut nets = Vec::new();
    let mut instances = Vec::new();
    let mut skipped = Vec::new();
    for item in &module.items {
        match item {
            UItem::Param { param } => params.push(summarise_param(param)),
            UItem::Net { net } => nets.push(NetSummary {
                name: net.name.clone(),
                net_type: format!("{:?}", net.net_type).to_lowercase(),
                packed: net.packed.as_ref().map(ToString::to_string),
                unpacked: net.unpacked.as_ref().map(ToString::to_string),
            }),
            // A non-ANSI body declaration filling in a header port. Shown on the
            // port itself so both header styles read the same way.
            UItem::PortDecl { name, dir, net_type, packed, .. } => {
                if let Some(port) = ports.iter_mut().find(|p| p.name == *name) {
                    port.dir = Some(format!("{dir:?}").to_lowercase());
                    if port.net_type.is_none() {
                        port.net_type = net_type.map(|t| format!("{t:?}").to_lowercase());
                    }
                    if port.packed.is_none() {
                        port.packed = packed.as_ref().map(ToString::to_string);
                    }
                }
            }
            UItem::Defparam { span, .. } => {
                skipped.push(format!("defparam at {}", design.files.render(*span)));
            }
            UItem::Inst { inst } => {
                let (style, count) = match &inst.conns {
                    UConns::Empty => ("none", 0),
                    UConns::Positional { values } => ("positional", values.len()),
                    UConns::Named { conns } => ("named", conns.len()),
                    UConns::Wildcard { conns } => ("wildcard", conns.len()),
                };
                let param_overrides = match &inst.param_overrides {
                    UParamOverrides::Empty => Vec::new(),
                    UParamOverrides::Positional { values } => {
                        values.iter().map(ToString::to_string).collect()
                    }
                    UParamOverrides::Named { values } => {
                        values.iter().map(|(n, v)| format!(".{n}({v})")).collect()
                    }
                };
                instances.push(InstanceSummary {
                    module: inst.module_name.clone(),
                    name: inst.name.clone(),
                    connection_style: style,
                    connection_count: count,
                    param_overrides,
                });
            }
            UItem::Unsupported { construct, span } => {
                skipped.push(format!("{construct} at {}", design.files.render(*span)));
            }
            _ => {}
        }
    }

    ModuleSummary {
        name: module.name.clone(),
        ansi_header: module.ansi_header,
        location: design.files.render(module.span),
        params,
        ports,
        nets,
        instances,
        skipped,
    }
}
