//! What the generators write.
//!
//! Snapshots, because a testbench is text that a person reads and edits: a
//! change to it should be looked at, not merely compiled.

use rtlscope_sv::ParseOptions;
use rtlscope_tb::{Flavor, TbOptions, generate_with};

fn generated(fixture: &str, top: Option<&str>, flavor: Flavor, options: TbOptions) -> String {
    let path = rtlscope_fixtures::path(fixture);
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, top);
    let design = design.expect("elaborates");

    // A fixed stand-in, so the snapshot does not carry this machine's paths.
    let options = TbOptions { sources: vec!["<the sources>".into()], ..options };
    let made = generate_with(&design, design.top, &options, flavor).expect("a testbench");

    // The toolchain files are the same constants whatever the design, and
    // hundreds of lines each; carrying them in two goldens would drown the
    // harness actually under review. What they *do* is tested by running them,
    // in `tests/drawn.rs`.
    let boilerplate = rtlscope_tb::toolchain::files();
    let mut out = String::new();
    for (name, contents) in &made.files {
        if boilerplate.iter().any(|(other, _)| other == name) {
            out.push_str(&format!("=== {name} === (generated toolchain, not shown)\n"));
            continue;
        }
        out.push_str(&format!("=== {name} ===\n{contents}\n"));
    }
    if !made.notes.is_empty() {
        out.push_str("=== notes ===\n");
        for note in &made.notes {
            out.push_str(&format!("{note}\n"));
        }
    }
    out
}

#[test]
fn a_cocotb_harness_for_a_synchronously_reset_counter() {
    insta::assert_snapshot!(generated(
        "counter.sv",
        None,
        Flavor::Cocotb,
        TbOptions { cycles: 100, ..TbOptions::default() }
    ));
}

/// An asynchronous reset, and a parameter shrunk to make the run short.
#[test]
fn a_cocotb_harness_carries_the_reset_kind_and_the_overrides() {
    insta::assert_snapshot!(generated(
        "pipeline3.sv",
        Some("pipeline3"),
        Flavor::Cocotb,
        TbOptions { cycles: 50, overrides: vec![("W".into(), 4)], ..TbOptions::default() }
    ));
}

#[test]
fn a_plain_systemverilog_harness_is_self_contained() {
    insta::assert_snapshot!(generated(
        "counter.sv",
        None,
        Flavor::Sv,
        TbOptions { cycles: 40, ..TbOptions::default() }
    ));
}

/// A parameter that is not there is refused, rather than written into a file
/// that will not compile.
#[test]
fn an_override_of_something_that_is_not_a_parameter_is_refused() {
    let path = rtlscope_fixtures::path("counter.sv");
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let design = rtlscope_elab::elaborate(&uir, None).0.expect("elaborates");

    let options = TbOptions { overrides: vec![("NOPE".into(), 1)], ..TbOptions::default() };
    let error = generate_with(&design, design.top, &options, Flavor::Cocotb)
        .expect_err("an unknown parameter is an error");
    assert!(error.to_string().contains("NOPE"), "{error}");
}

/// The runner clears an earlier run's waveform before starting, so a run that
/// produces none cannot hand the reader the last one's. It can only clear what
/// it can name, and the names live in the script — so the two lists have to
/// stay the same list.
#[test]
fn every_waveform_the_script_can_name_is_one_the_runner_knows_to_clear() {
    let path = rtlscope_fixtures::path("counter.sv");
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let design = rtlscope_elab::elaborate(&uir, None).0.expect("elaborates");
    let top = design.modules[design.top].base_name.clone();

    // Both shapes of the script, because which name it writes depends on the
    // options: a scoped dump is an `.fst` of its own, and a plain one is
    // whichever of two the simulator produces. The runner clears before it
    // knows which, so it has to cover all of them.
    let mut script = String::new();
    for options in [
        TbOptions::default(),
        TbOptions { dump_scope: Some("dut.u_inner".into()), ..TbOptions::default() },
    ] {
        let made =
            generate_with(&design, design.top, &options, Flavor::Cocotb).expect("a testbench");
        let (_, text) = made
            .files
            .iter()
            .find(|(name, _)| name == "run.py")
            .expect("the script that names the waveform");
        script.push_str(text);
    }

    let names = rtlscope_tb::cocotb::dump_names(&top);
    assert!(!names.is_empty(), "a run that clears nothing cannot be trusted twice");
    for name in names {
        // By file name: the script builds the directory part itself, out of
        // `here`, so only the last component is written there literally.
        let spelled =
            name.file_name().expect("every candidate is a file").to_string_lossy().into_owned();
        assert!(script.contains(&spelled), "`{spelled}` is not named in run.py:\n{script}");
    }
}

/// Only Verilator needs the VPI archive and the interpreter that matches it.
/// The script asks for both under that engine alone, so Icarus plays a pattern
/// on a machine that has no C++ compiler.
#[test]
fn only_verilator_is_asked_for_the_vpi_archive_and_the_matching_interpreter() {
    let script = generated("counter.sv", None, Flavor::Cocotb, TbOptions::default());
    let asked = "if engine == \"verilator\":\n    site.check_interpreter(root)\n    vpi.ensure()\n";
    assert!(script.contains(asked), "the checks are not under the engine:\n{script}");
    assert!(script.contains("root = site.prepend_path(engine)"), "the engine decides the path");
}
