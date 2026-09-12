//! Reading the JSON Yosys writes.
//!
//! `write_json` emits the whole design — modules, ports, cells, nets — and
//! this takes the part of it that is a *fact about the source* rather than a
//! fact about Yosys's internal representation. Cells named `$add`, nets named
//! `$0\count[7:0]`, the processes `proc` turned into logic: none of that is
//! something RTLScope claims anything about, so none of it is read.
//!
//! Two details of the format are worth knowing, because both would otherwise
//! be guessed at wrongly.
//!
//! **A specialised module is named by mangling.** `hier_alu` built with `W=32`
//! comes back as `` $paramod\hier_alu\W=s32'000...100000 ``, and a second form
//! `` $paramod$<sha1>\hier_regfile `` is used when the parameters do not fit in
//! a name. Rather than unpick either, this reads the `hdlname` attribute, which
//! is Yosys's own record of what the module was called in the source; the
//! mangling is only unpicked when that attribute is absent.
//!
//! **A parameter's value is a bit string, and Yosys does not say whether it
//! meant it as signed.** So both readings are kept and a comparison counts as
//! agreeing if either matches — rather than reporting a disagreement that is
//! really a difference of convention.

use std::collections::BTreeMap;

use rtlscope_ir::PortDir;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum NetlistError {
    #[error("could not read `{path}`: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("`{path}` is not JSON Yosys wrote: {reason}")]
    Parse { path: String, reason: String },
    #[error(
        "`{0}` has no modules in it. `write_json` after `read_verilog` alone writes nothing \
         useful; run `hierarchy -top <TOP>` and `proc` first."
    )]
    Empty(String),
}

/// One design, as Yosys sees it.
#[derive(Debug, Clone)]
pub struct Netlist {
    /// The version string Yosys stamps into the file.
    pub creator: String,
    pub modules: Vec<Module>,
}

#[derive(Debug, Clone)]
pub struct Module {
    /// The name Yosys uses, mangled when the module was specialised.
    pub name: String,
    /// The name in the source: Yosys's `hdlname` when it left one.
    pub base_name: String,
    /// Yosys's `src` attribute, `file:line.col-line.col`.
    pub location: Option<String>,
    pub is_top: bool,
    pub params: Vec<(String, ParamValue)>,
    pub ports: Vec<Port>,
    /// Cells that instantiate another module, as opposed to Yosys's own logic.
    pub instances: Vec<Instance>,
    /// How many cells in all, including the ones Yosys made.
    pub cells: usize,
    /// Nets the source named. Yosys's own are left out.
    pub nets: Vec<Net>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub name: String,
    pub dir: PortDir,
    pub width: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub name: String,
    /// The module it instantiates, by its source name.
    pub of: String,
    /// The same, as Yosys writes it — mangled when specialised.
    pub of_raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Net {
    pub name: String,
    pub width: u32,
}

/// A parameter's value, read both ways because Yosys does not say which it
/// meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamValue {
    Bits {
        signed: i64,
        unsigned: u64,
        width: u32,
    },
    /// More than 64 bits, so neither reading is a number here.
    Wide(String),
    /// Not bits at all — a string parameter, which RTLScope does not model.
    Text(String),
    /// Some bit was `x` or `z`.
    Unknown(String),
}

impl ParamValue {
    /// Whether this is the number the IR says it is, under either reading.
    pub fn is(&self, value: i64) -> bool {
        match self {
            ParamValue::Bits { signed, unsigned, .. } => {
                *signed == value || u64::try_from(value).is_ok_and(|wanted| *unsigned == wanted)
            }
            _ => false,
        }
    }

    pub fn shown(&self) -> String {
        match self {
            ParamValue::Bits { signed, unsigned, .. } if *signed < 0 => {
                format!("{signed} (or {unsigned} unsigned)")
            }
            ParamValue::Bits { signed, .. } => signed.to_string(),
            ParamValue::Wide(bits) => format!("{} bits", bits.len()),
            ParamValue::Text(text) => format!("\"{text}\""),
            ParamValue::Unknown(bits) => format!("{bits} (undriven)"),
        }
    }
}

pub fn read(path: &std::path::Path) -> Result<Netlist, NetlistError> {
    let text = std::fs::read_to_string(path)
        .map_err(|source| NetlistError::Io { path: path.display().to_string(), source })?;
    let netlist = parse(&text)
        .map_err(|reason| NetlistError::Parse { path: path.display().to_string(), reason })?;
    if netlist.modules.is_empty() {
        return Err(NetlistError::Empty(path.display().to_string()));
    }
    Ok(netlist)
}

pub fn parse(text: &str) -> Result<Netlist, String> {
    let raw: RawNetlist = serde_json::from_str(text).map_err(|error| error.to_string())?;
    Ok(Netlist {
        creator: raw.creator,
        modules: raw.modules.into_iter().map(|(name, module)| convert(name, module)).collect(),
    })
}

fn convert(name: String, raw: RawModule) -> Module {
    let base_name = raw
        .attributes
        .get("hdlname")
        .and_then(|value| value.as_str())
        // `hdlname` is a space-separated path for a module Yosys renamed more
        // than once; the last entry is the one it ended up being built from.
        .and_then(|text| text.split_whitespace().next_back())
        .map(str::to_string)
        .unwrap_or_else(|| unmangle(&name).to_string());

    let mut ports: Vec<Port> = raw
        .ports
        .into_iter()
        .map(|(name, port)| Port {
            name,
            dir: direction(&port.direction),
            width: port.bits.len() as u32,
        })
        .collect();
    ports.sort_by(|a, b| a.name.cmp(&b.name));

    let cells = raw.cells.len();
    let mut instances: Vec<Instance> = raw
        .cells
        .into_iter()
        // A cell whose type starts with `$` and is not a specialisation is one
        // of Yosys's own: an adder, a multiplexer, a flip-flop it made out of a
        // process. The source did not write it, so it is not compared.
        .filter(|(_, cell)| !cell.kind.starts_with('$') || cell.kind.starts_with("$paramod"))
        .map(|(name, cell)| Instance {
            name,
            of: unmangle(&cell.kind).to_string(),
            of_raw: cell.kind,
        })
        .collect();
    instances.sort_by(|a, b| a.name.cmp(&b.name));

    let mut nets: Vec<Net> = raw
        .netnames
        .into_iter()
        .filter(|(_, net)| net.hide_name == 0)
        .map(|(name, net)| Net { name, width: net.bits.len() as u32 })
        .collect();
    nets.sort_by(|a, b| a.name.cmp(&b.name));

    let mut params: Vec<(String, ParamValue)> = raw
        .parameter_default_values
        .into_iter()
        .map(|(name, value)| (name, value_of(&value)))
        .collect();
    params.sort_by(|a, b| a.0.cmp(&b.0));

    Module {
        is_top: raw.attributes.contains_key("top"),
        location: raw.attributes.get("src").and_then(|v| v.as_str()).map(str::to_string),
        name,
        base_name,
        params,
        ports,
        instances,
        cells,
        nets,
    }
}

/// The source name inside a mangled one.
///
/// `$paramod\hier_alu\W=s32'...` and `$paramod$<sha1>\hier_regfile` both carry
/// the module's own name as the first segment that is not one of Yosys's, so
/// that is what this takes. A name Yosys did not mangle comes back unchanged.
pub fn unmangle(name: &str) -> &str {
    name.split('\\')
        .find(|segment| !segment.is_empty() && !segment.starts_with('$'))
        .unwrap_or(name)
}

fn direction(text: &str) -> PortDir {
    match text {
        "output" => PortDir::Output,
        "inout" => PortDir::Inout,
        // Yosys writes exactly these three; anything else is a file this did
        // not write, and an input is the reading that hides the least.
        _ => PortDir::Input,
    }
}

fn value_of(value: &serde_json::Value) -> ParamValue {
    if let Some(number) = value.as_i64() {
        return ParamValue::Bits { signed: number, unsigned: number as u64, width: 32 };
    }
    let Some(text) = value.as_str() else {
        return ParamValue::Text(value.to_string());
    };
    if text.is_empty() || !text.bytes().all(|b| matches!(b, b'0' | b'1' | b'x' | b'z')) {
        // Yosys writes a string parameter as the string itself, which is
        // indistinguishable from a bit vector only when the string happens to
        // be all noughts and ones. Everything else is plainly text.
        return ParamValue::Text(text.to_string());
    }
    if text.bytes().any(|b| b == b'x' || b == b'z') {
        return ParamValue::Unknown(text.to_string());
    }
    let width = text.len() as u32;
    if width > 64 {
        return ParamValue::Wide(text.to_string());
    }
    let unsigned = u64::from_str_radix(text, 2).unwrap_or(0);
    // Two's complement over the width Yosys wrote, which is how a Verilog
    // parameter without an explicit type is read.
    let signed = if width < 64 && text.starts_with('1') {
        unsigned as i64 - (1i64 << width)
    } else {
        unsigned as i64
    };
    ParamValue::Bits { signed, unsigned, width }
}

// ------------------------------------------------------- the file itself ---

#[derive(Debug, Deserialize)]
struct RawNetlist {
    #[serde(default)]
    creator: String,
    #[serde(default)]
    modules: BTreeMap<String, RawModule>,
}

#[derive(Debug, Deserialize)]
struct RawModule {
    #[serde(default)]
    attributes: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    parameter_default_values: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    ports: BTreeMap<String, RawPort>,
    #[serde(default)]
    cells: BTreeMap<String, RawCell>,
    #[serde(default)]
    netnames: BTreeMap<String, RawNet>,
}

#[derive(Debug, Deserialize)]
struct RawPort {
    direction: String,
    #[serde(default)]
    bits: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawCell {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct RawNet {
    #[serde(default)]
    bits: Vec<serde_json::Value>,
    #[serde(default)]
    hide_name: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_specialised_module_keeps_the_name_the_source_gave_it() {
        assert_eq!(
            unmangle(r"$paramod\hier_alu\W=s32'00000000000000000000000000100000"),
            "hier_alu"
        );
        assert_eq!(unmangle(r"$paramod$6a8e005b\hier_regfile"), "hier_regfile");
        assert_eq!(unmangle("hier_ctrl"), "hier_ctrl");
        assert_eq!(unmangle(r"\escaped"), "escaped");
        // One of Yosys's own cells is not a module and has no source name.
        assert_eq!(unmangle("$dff"), "$dff");
    }

    #[test]
    fn a_parameter_reads_as_a_number_either_way_round() {
        let value = value_of(&serde_json::json!("00000000000000000000000000100000"));
        assert!(value.is(32));
        assert_eq!(value.shown(), "32");

        // All ones over 32 bits: -1 signed, 4294967295 unsigned. Yosys does not
        // say which it meant, so both count.
        let value = value_of(&serde_json::json!("11111111111111111111111111111111"));
        assert!(value.is(-1), "{value:?}");
        assert!(value.is(4_294_967_295), "{value:?}");
    }

    #[test]
    fn a_string_parameter_is_not_pretended_to_be_a_number() {
        let value = value_of(&serde_json::json!("hello"));
        assert_eq!(value, ParamValue::Text("hello".into()));
        assert!(!value.is(0), "a string is not zero");
    }

    #[test]
    fn an_undriven_parameter_stays_undriven() {
        let value = value_of(&serde_json::json!("0000xxxx"));
        assert!(matches!(value, ParamValue::Unknown(_)));
        assert!(!value.is(0));
    }

    #[test]
    fn a_file_that_is_not_a_netlist_says_so_rather_than_coming_back_empty() {
        assert!(parse("not json at all").is_err());
        assert_eq!(parse(r#"{"creator":"x"}"#).unwrap().modules.len(), 0);
    }
}
