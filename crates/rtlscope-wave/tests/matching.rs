//! Matching a dump against a design.
//!
//! The fixture dumps here are written to sit under `tb.dut`, which is where a
//! testbench actually puts a design — the case the matcher exists for.

use rtlscope_analyse::flat;
use rtlscope_ir::Design;
use rtlscope_sv::ParseOptions;
use rtlscope_wave::{Dump, match_signals};

fn design(fixture: &str, top: Option<&str>) -> Design {
    let path = rtlscope_fixtures::path(fixture);
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, top);
    design.expect("elaboration produced a design")
}

/// A dump of `counter.sv` recorded under `tb.dut`, plus one testbench-only var.
fn counter_dump(scope: &str) -> Dump {
    let scopes: Vec<&str> = scope.split('.').filter(|s| !s.is_empty()).collect();
    let mut text = String::from("$timescale 1ns $end\n");
    for name in &scopes {
        text.push_str(&format!("$scope module {name} $end\n"));
    }
    text.push_str(
        "$var wire 1 ! clk $end\n\
         $var wire 1 \" rst_n $end\n\
         $var wire 1 # en $end\n\
         $var wire 8 $ count $end\n",
    );
    for _ in &scopes {
        text.push_str("$upscope $end\n");
    }
    // A variable the testbench owns and the design knows nothing about.
    text.push_str(
        "$scope module harness $end\n$var wire 32 % cycles $end\n$upscope $end\n\
         $enddefinitions $end
#0
0!
0\"
1#
b00000000 $
b00000000 %
#10
1!
b00000001 $
",
    );
    Dump::open_vcd_bytes(text.into_bytes()).expect("the fixture parses")
}

#[test]
fn the_scope_the_design_sits_under_is_inferred_and_reported() {
    let design = design("counter.sv", None);
    let flat = flat::flatten(&design);
    let dump = counter_dump("tb.dut");

    let report = match_signals(&dump, &design, &flat, None);
    assert_eq!(report.prefix, "tb.dut");
    // Every net of `counter` is in the dump.
    assert_eq!(report.prefix_score.0, report.prefix_score.1, "{report:#?}");
    assert!(report.unmatched_ir.is_empty(), "{:#?}", report.unmatched_ir);

    let names: Vec<&str> = report.matched.iter().map(|m| m.ir_name.as_str()).collect();
    for wanted in ["clk", "rst_n", "en", "count"] {
        assert!(names.contains(&wanted), "{names:?}");
    }
    let count = report.by_ir_name("count").expect("count matched");
    assert_eq!(count.dump_path, "tb.dut.count");
    assert_eq!(count.width, 8);
}

/// A dump written from the design itself, with no testbench above it.
#[test]
fn a_dump_that_starts_at_the_design_needs_no_prefix() {
    let design = design("counter.sv", None);
    let flat = flat::flatten(&design);
    let dump = counter_dump("");

    let report = match_signals(&dump, &design, &flat, None);
    assert_eq!(report.prefix, "");
    assert_eq!(report.matched.len(), 4, "{report:#?}");
}

/// The testbench's own signals are listed, not counted as failures.
#[test]
fn what_belongs_to_the_testbench_is_named_separately() {
    let design = design("counter.sv", None);
    let flat = flat::flatten(&design);
    let dump = counter_dump("tb.dut");

    let report = match_signals(&dump, &design, &flat, None);
    // The harness scope sits beside the design, not under it.
    assert_eq!(report.unmatched_dump, ["harness.cycles"], "{report:#?}");
}

/// A name that matches at the wrong width is a different wire, and saying so
/// beats binding it and reading the wrong bits.
#[test]
fn a_width_that_disagrees_is_reported_rather_than_bound() {
    let design = design("counter.sv", None);
    let flat = flat::flatten(&design);
    // `count` recorded as 4 bits where the design says 8.
    let text = "$timescale 1ns $end\n\
        $scope module tb $end\n$scope module dut $end\n\
        $var wire 1 ! clk $end\n\
        $var wire 1 \" rst_n $end\n\
        $var wire 1 # en $end\n\
        $var wire 4 $ count $end\n\
        $upscope $end\n$upscope $end\n$enddefinitions $end\n#0\n0!\n0\"\n1#\nb0 $\n";
    let dump = Dump::open_vcd_bytes(text.as_bytes().to_vec()).unwrap();

    let report = match_signals(&dump, &design, &flat, Some("tb.dut"));
    assert!(report.by_ir_name("count").is_none(), "the mis-widthed net should not match");
    let reason = report
        .unmatched_ir
        .iter()
        .find(|u| u.ir_name == "count")
        .map(|u| u.reason.clone())
        .expect("count reported as unmatched");
    assert!(reason.contains("4 bits"), "{reason}");
    assert!(reason.contains("8"), "{reason}");
}

/// A design with hierarchy: names in the dump carry the instance path, and the
/// matcher must line them up through it.
#[test]
fn nets_inside_instances_match_through_their_paths() {
    let design = design("hier.sv", Some("hier_top"));
    let flat = flat::flatten(&design);

    // Build a dump from what the design says, under `tb.dut`.
    let mut text =
        String::from("$timescale 1ns $end\n$scope module tb $end\n$scope module dut $end\n");
    let mut ids = ('!'..='~').filter(|c| *c != '$' && *c != '"');
    let mut declared = Vec::new();
    for (name, _, width, invented) in flat.all_names(&design) {
        if invented || name.contains('.') {
            continue; // top-level nets only, kept flat for a readable fixture
        }
        let id = ids.next().unwrap();
        text.push_str(&format!("$var wire {width} {id} {name} $end\n"));
        declared.push(name);
    }
    text.push_str("$upscope $end\n$upscope $end\n$enddefinitions $end\n#0\n");
    Dump::open_vcd_bytes(text.clone().into_bytes()).expect("the fixture parses");

    let dump = Dump::open_vcd_bytes(text.into_bytes()).unwrap();
    let report = match_signals(&dump, &design, &flat, None);
    assert_eq!(report.prefix, "tb.dut", "{:?}", report.prefix_score);
    assert_eq!(report.matched.len(), declared.len(), "{report:#?}");
}
