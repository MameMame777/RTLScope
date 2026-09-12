//! Printing what the two front ends agree and disagree about.
//!
//! The verdict leads, because that is the whole question. But the count of
//! things that *could not* be compared is printed just as prominently, since a
//! check that compared nothing also reports no disagreements, and a green line
//! that means "I looked at four ports" is worse than no line at all.

use std::fmt::Write as _;

use rtlscope_yosys::CrossCheck;

pub fn check_text(report: &CrossCheck) -> String {
    let mut out = String::new();
    if !report.creator.is_empty() {
        let _ = writeln!(out, "{}\n", report.creator);
    }
    let _ = writeln!(
        out,
        "{} module(s) compared, {} fact(s) checked",
        report.modules.len(),
        report.checked
    );

    if report.agrees() {
        out.push_str("\nRTLScope and the netlist agree about every one of them.\n");
    } else {
        let _ = writeln!(out, "\n{} disagreement(s)", report.differences.len());
        for difference in &report.differences {
            let _ = writeln!(
                out,
                "  {:<20} {:<10} {:<24} here {:<18} netlist {}",
                difference.module,
                format!("{:?}", difference.kind).to_lowercase(),
                difference.name,
                difference.ir,
                difference.netlist
            );
        }
    }

    if !report.only_in_ir.is_empty() {
        let _ = writeln!(
            out,
            "\n{} module(s) only RTLScope has: {}",
            report.only_in_ir.len(),
            report.only_in_ir.join(", ")
        );
    }
    if !report.only_in_netlist.is_empty() {
        let _ = writeln!(
            out,
            "\n{} module(s) only the netlist has: {}",
            report.only_in_netlist.len(),
            report.only_in_netlist.join(", ")
        );
        out.push_str(
            "  Yosys was given files RTLScope was not, or read a construct RTLScope skipped the \
             module over.\n",
        );
    }

    if !report.modules.is_empty() {
        out.push('\n');
        for module in &report.modules {
            let verdict = if module.differences == 0 {
                "agree".to_string()
            } else {
                format!("{} differ", module.differences)
            };
            let _ = writeln!(
                out,
                "  {:<24} {:>4} fact(s)  {:<12} {}",
                module.name,
                module.checked,
                verdict,
                module.location.as_deref().unwrap_or("")
            );
        }
    }

    // Last and unmissable: a clean result over nothing compared is not a clean
    // result.
    if report.notes.is_empty() {
        out.push_str("\nEverything either tool knows about was compared.\n");
    } else {
        let _ = writeln!(out, "\n{} thing(s) could not be compared", report.notes.len());
        for note in &report.notes {
            let _ = writeln!(out, "  {note}");
        }
    }
    out
}

/// The command that produces the netlist this reads.
///
/// Printed rather than run: keeping Yosys an input means it stays optional, and
/// a CI job that already has it can put this between two steps it already runs.
pub fn yosys_command(sources: &[String], top: &str, out: &str) -> String {
    format!(
        "yosys -p \"read_verilog -sv {}; hierarchy -top {top}; proc; write_json {out}\"",
        sources.join(" ")
    )
}
