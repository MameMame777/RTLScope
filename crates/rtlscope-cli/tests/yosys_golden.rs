//! What the cross-check prints.
//!
//! Both the clean verdict and a dirty one, because the two have to read
//! differently at a glance — and because a renderer that only ever ran on
//! agreement is a renderer nobody has seen do its job.

use rtlscope_sv::ParseOptions;

fn design(fixture: &str, top: &str) -> rtlscope_ir::Design {
    let path = rtlscope_fixtures::path(fixture);
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, Some(top));
    design.expect("elaboration produced a design")
}

fn netlist(name: &str) -> rtlscope_yosys::Netlist {
    rtlscope_yosys::read(&rtlscope_fixtures::netlist(name)).expect("the fixture parses")
}

#[test]
fn two_front_ends_agreeing_reads_as_a_verdict() {
    let report = rtlscope_yosys::check(&design("hier.sv", "hier_top"), &netlist("hier.json"));
    insta::assert_snapshot!(rtlscope_cli::cmd::yosys::check_text(&report));
}

#[test]
fn a_disagreement_names_what_each_side_said() {
    let mut netlist = netlist("hier.json");
    let module = netlist.modules.iter_mut().find(|m| m.base_name == "hier_top").expect("the top");
    module.ports.iter_mut().find(|p| p.name == "result").expect("`result`").width = 16;
    module.instances.retain(|inst| inst.name != "u_ctrl");

    let report = rtlscope_yosys::check(&design("hier.sv", "hier_top"), &netlist);
    assert!(!report.agrees());
    insta::assert_snapshot!(rtlscope_cli::cmd::yosys::check_text(&report));
}

/// The command is printed rather than run, so it has to be one that works.
#[test]
fn the_command_it_suggests_is_the_one_that_makes_the_netlist() {
    let command = rtlscope_cli::cmd::yosys::yosys_command(
        &["a.sv".to_string(), "b.sv".to_string()],
        "top",
        "out.json",
    );
    assert_eq!(
        command,
        "yosys -p \"read_verilog -sv a.sv b.sv; hierarchy -top top; proc; write_json out.json\""
    );
    // `proc` is the part that is easy to leave out and that `write_json`
    // refuses without.
    assert!(command.contains("; proc;"), "{command}");
}
