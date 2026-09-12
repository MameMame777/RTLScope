//! What the wave commands print.
//!
//! The renderers are golden-tested rather than the binary, so a change to the
//! wording shows up as a diff to approve instead of as a broken shell script.

use rtlscope_wave::Dump;
use rtlscope_wave::decode::{Binding, ResolvedBindings};

/// A small I2C write, as a dump: the shape a person is most likely to point
/// the tool at first.
fn sccb_dump() -> Dump {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sccb.vcd");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Dump::open_vcd_bytes(bytes).expect("the fixture parses")
}

#[test]
fn a_decoded_transfer_reads_as_a_register_write() {
    let mut dump = sccb_dump();
    let decoder = rtlscope_wave::decode::by_name("i2c").expect("the i2c decoder");
    let bindings = [Binding::parse("scl=tb.scl").unwrap(), Binding::parse("sda=tb.sda").unwrap()];
    let resolved =
        ResolvedBindings::resolve(&mut dump, decoder.channels(), &bindings).expect("bindings");
    let report = decoder.decode(&dump, &resolved);
    insta::assert_snapshot!(rtlscope_cli::cmd::wave::decode_text(&report));
}

#[test]
fn the_protocols_are_listed_with_what_they_need() {
    insta::assert_snapshot!(rtlscope_cli::cmd::wave::protocols_text());
}

/// `pipeline3.sv` run over a four-on four-off burst, recorded under `tb.dut`.
#[test]
fn the_stages_read_as_a_diagonal_down_the_cycles() {
    let path = rtlscope_fixtures::path("pipeline3.sv");
    let (uir, _) = rtlscope_sv::lower_files(&[path], &rtlscope_sv::ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, Some("pipeline3"));
    let design = design.expect("elaboration produced a design");

    let bytes =
        std::fs::read(rtlscope_fixtures::wave("pipeline3.vcd")).expect("the fixture is there");
    let mut dump = Dump::open_vcd_bytes(bytes).expect("the fixture parses");
    let flat = rtlscope_analyse::flat::flatten(&design);
    let matches = rtlscope_wave::match_signals(&dump, &design, &flat, None);

    let report = rtlscope_analyse::pipeline::analyse(&design);
    let domain = report.domains.first().expect("a clock domain");
    let cycles =
        rtlscope_wave::stages::cycles(&mut dump, &matches, &domain.clock).expect("a clock");
    let layout = rtlscope_wave::Layout::window(0, 32);
    let view = rtlscope_wave::stages::occupancy(&mut dump, &matches, domain, &cycles, &layout);

    insta::assert_snapshot!(rtlscope_cli::cmd::wave::stages_text(&view));
}

/// A run with one failure in it, against the dump it produced.
#[test]
fn a_failing_test_reads_as_a_moment_in_the_dump() {
    let run = rtlscope_tb::results::read(&rtlscope_fixtures::wave("results.xml"))
        .expect("the fixture parses");
    let bytes =
        std::fs::read(rtlscope_fixtures::wave("pipeline3.vcd")).expect("the fixture is there");
    let dump = Dump::open_vcd_bytes(bytes).expect("the fixture parses");

    insta::assert_snapshot!(rtlscope_cli::cmd::tb::results_text(
        &run,
        Some(&dump),
        "pipeline3.vcd"
    ));
}

/// A moment past where the dump stops is a place to look that does not exist,
/// and handing over the tick without saying so is how someone spends an
/// afternoon scrolling to the end of a waveform that was cut short.
#[test]
fn a_moment_past_the_end_of_the_dump_is_called_out() {
    let run = rtlscope_tb::results::parse(
        r#"<testcase name="late" classname="t" sim_time_ns="9000.0"><failure msg="boom" /></testcase>"#,
    );
    let bytes = std::fs::read(rtlscope_fixtures::wave("pipeline3.vcd")).expect("the fixture");
    let dump = Dump::open_vcd_bytes(bytes).expect("the fixture parses");

    let text = rtlscope_cli::cmd::tb::results_text(&run, Some(&dump), "pipeline3.vcd");
    assert!(text.contains("past the end of pipeline3.vcd"), "{text}");
    assert!(text.contains("stops at 405"), "{text}");
}
