//! RTLScope's IR against a real Yosys netlist.
//!
//! The netlists here were written by Yosys 0.66 rather than by hand, which is
//! the only way this test means anything: the point of the check is that a
//! *second* tool read the same file, and a fixture written to match RTLScope's
//! idea of the answer would agree with it by construction.
//!
//! They were produced with
//!
//! ```text
//! yosys -p "read_verilog -sv <fixture>; hierarchy -top <TOP>; proc; write_json <out>"
//! ```
//!
//! The disagreement cases mutate a real netlist in memory, so that what is
//! being tested is the comparison rather than a second hand-written file.

use rtlscope_ir::Design;
use rtlscope_sv::ParseOptions;
use rtlscope_yosys::netlist::{Netlist, ParamValue};
use rtlscope_yosys::{Kind, check};

fn design(fixture: &str, top: &str) -> Design {
    let path = rtlscope_fixtures::path(fixture);
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, Some(top));
    design.expect("elaboration produced a design")
}

fn netlist(name: &str) -> Netlist {
    let path = rtlscope_fixtures::netlist(name);
    rtlscope_yosys::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The whole point, in one assertion: two front ends, one file, no argument.
#[test]
fn two_front_ends_agree_about_a_hierarchy() {
    let report = check(&design("hier.sv", "hier_top"), &netlist("hier.json"));

    assert!(report.agrees(), "{:#?}", report.differences);
    assert_eq!(report.modules.len(), 4);
    assert!(report.checked > 50, "only {} fact(s) compared", report.checked);
    assert!(report.creator.starts_with("Yosys"), "{}", report.creator);

    // A memory is a net here and lives elsewhere in the netlist. That is not a
    // disagreement, and it is not silence either.
    assert!(report.notes.iter().any(|note| note.contains("memor")), "{:#?}", report.notes);
}

/// The trap this comparison sets for itself: `W=8` and `W=16` compared the
/// wrong way round would report every width as wrong.
#[test]
fn every_specialisation_is_paired_by_what_its_parameters_came_out_as() {
    let report = check(&design("params.sv", "params_top"), &netlist("params.json"));

    assert!(report.agrees(), "{:#?}", report.differences);
    let mut names: Vec<&str> = report.modules.iter().map(|m| m.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "params_sub$W=16",
            "params_sub$W=8",
            "params_subsub$W=16",
            "params_subsub$W=8",
            "params_top"
        ]
    );
    assert!(report.notes.is_empty(), "{:#?}", report.notes);
}

/// A check that never disagrees is not a check, so here is one that does.
#[test]
fn a_port_of_the_wrong_width_is_noticed() {
    let mut netlist = netlist("hier.json");
    let module = netlist
        .modules
        .iter_mut()
        .find(|m| m.base_name == "hier_top")
        .expect("the top is in the netlist");
    let port = module.ports.iter_mut().find(|p| p.name == "result").expect("`result`");
    port.width = 16;

    let report = check(&design("hier.sv", "hier_top"), &netlist);
    assert!(!report.agrees());
    let widths: Vec<&rtlscope_yosys::Difference> =
        report.differences.iter().filter(|d| d.kind == Kind::Width).collect();
    assert_eq!(widths.len(), 1, "{:#?}", report.differences);
    assert_eq!(widths[0].name, "result");
    assert_eq!(widths[0].ir, "32 bit(s)");
    assert_eq!(widths[0].netlist, "16 bit(s)");
}

/// Parameter evaluation is where a front end is most easily and most quietly
/// wrong, which makes it the fact most worth a second opinion.
#[test]
fn a_parameter_that_evaluated_differently_is_noticed() {
    let mut netlist = netlist("hier.json");
    let module = netlist.modules.iter_mut().find(|m| m.base_name == "hier_alu").expect("hier_alu");
    for (_, value) in module.params.iter_mut() {
        *value = ParamValue::Bits { signed: 64, unsigned: 64, width: 32 };
    }

    let report = check(&design("hier.sv", "hier_top"), &netlist);
    let params: Vec<&rtlscope_yosys::Difference> =
        report.differences.iter().filter(|d| d.kind == Kind::Parameter).collect();
    assert_eq!(params.len(), 1, "{:#?}", report.differences);
    assert_eq!(params[0].name, "W");
    assert_eq!(params[0].ir, "32");
    assert_eq!(params[0].netlist, "64");
}

#[test]
fn an_instance_the_netlist_does_not_have_is_noticed() {
    let mut netlist = netlist("hier.json");
    let module = netlist.modules.iter_mut().find(|m| m.base_name == "hier_top").expect("hier_top");
    module.instances.retain(|inst| inst.name != "u_alu");

    let report = check(&design("hier.sv", "hier_top"), &netlist);
    let instances: Vec<&rtlscope_yosys::Difference> =
        report.differences.iter().filter(|d| d.kind == Kind::Instance).collect();
    assert_eq!(instances.len(), 1, "{:#?}", report.differences);
    assert_eq!(instances[0].name, "u_alu");
    assert_eq!(instances[0].netlist, "—");
}

/// An IP with no source is drawn as a black box here and does not exist at all
/// in a netlist built from the same files. Neither tool is wrong, so it is a
/// note rather than a difference — but a note, not silence.
#[test]
fn a_module_with_no_source_is_not_a_disagreement() {
    let mut design = design("hier.sv", "hier_top");
    let (id, _) = design.module_by_name("hier_ctrl").expect("hier_ctrl");
    design.modules[id].is_blackbox = true;

    let mut netlist = netlist("hier.json");
    netlist.modules.retain(|module| module.base_name != "hier_ctrl");

    let report = check(&design, &netlist);
    assert!(report.only_in_ir.is_empty(), "{:?}", report.only_in_ir);
    assert!(
        report.notes.iter().any(|note| note.contains("hier_ctrl") && note.contains("black box")),
        "{:#?}",
        report.notes
    );
    // Its instance is still expected to be there, so removing the module alone
    // does not hide a real difference.
    assert!(report.differences.iter().all(|d| d.kind != Kind::Module), "{:#?}", report.differences);
}

/// A module the netlist has and the IR does not is the direction that matters
/// most: it means RTLScope skipped something.
#[test]
fn a_module_only_the_netlist_has_is_reported() {
    let mut design = design("hier.sv", "hier_top");
    let (id, _) = design.module_by_name("hier_ctrl").expect("hier_ctrl");
    // Renaming it is the cheapest way to make the IR not have it under the name
    // both tools would otherwise share.
    design.modules[id].base_name = "something_else".to_string();

    let report = check(&design, &netlist("hier.json"));
    assert_eq!(report.only_in_netlist, vec!["hier_ctrl"]);
    assert_eq!(report.only_in_ir, vec!["something_else"]);
    assert!(!report.agrees());
}

/// The netlist names the top, and getting that wrong makes everything under it
/// the right answer to a different question.
#[test]
fn the_top_is_compared_as_a_fact_of_its_own() {
    let mut design = design("hier.sv", "hier_top");
    let (id, _) = design.module_by_name("hier_ctrl").expect("hier_ctrl");
    design.top = id;

    let report = check(&design, &netlist("hier.json"));
    let tops: Vec<&rtlscope_yosys::Difference> =
        report.differences.iter().filter(|d| d.kind == Kind::Top).collect();
    assert_eq!(tops.len(), 1, "{:#?}", report.differences);
    assert_eq!(tops[0].ir, "hier_ctrl");
    assert_eq!(tops[0].netlist, "hier_top");
}
