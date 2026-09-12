//! The measured distance, and what it says next to the structural one.
//!
//! Note on fixtures: `waves/pipeline3.vcd` is **not** a run of `pipeline3.sv`.
//! It was built to make the stage-occupancy diagram show a diagonal, and its
//! `out_data` is a down-counter that owes nothing to `in_data` — measuring a
//! latency against it would be measuring the fixture. So the faithful pipeline
//! here is written by hand, which is the house rule for wave tests anyway.
//!
//! `waves/trace_demo.vcd` *is* a real run, and is used as one.

use std::collections::BTreeMap;

use rtlscope_analyse::flat::flatten;
use rtlscope_ir::Design;
use rtlscope_sv::ParseOptions;
use rtlscope_wave::latency::latency;
use rtlscope_wave::matching::match_signals;
use rtlscope_wave::stages::{Cycles, cycles};
use rtlscope_wave::{Dump, LatencyReport};

fn design(fixture: &str, top: Option<&str>) -> Design {
    let path = rtlscope_fixtures::path(fixture);
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    rtlscope_elab::elaborate(&uir, top).0.expect("elaborates")
}

/// A recording of `pipeline3` that actually behaves like one.
///
/// `in_data` takes a new value every other cycle; `out_data` takes the same
/// value three cycles later, which is what three registers do.
fn a_faithful_pipeline() -> Dump {
    let mut moments: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    let edge = |cycle: u64| cycle * 10;

    for cycle in 0..40u64 {
        moments.entry(edge(cycle)).or_default().push("1!".to_string());
        moments.entry(edge(cycle) + 5).or_default().push("0!".to_string());
    }
    moments.entry(0).or_default().push("b0000000000000000 a".to_string());
    moments.entry(0).or_default().push("b0000000000000000 b".to_string());

    for beat in 1..12u64 {
        let value = format!("b{beat:016b}");
        // In at an even cycle, out three later. The value is the same, which is
        // what makes the pairing mean anything.
        moments.entry(edge(beat * 2)).or_default().push(format!("{value} a"));
        moments.entry(edge(beat * 2 + 3)).or_default().push(format!("{value} b"));
    }

    let mut body = String::new();
    for (time, statements) in moments {
        body.push_str(&format!("#{time}\n"));
        for statement in statements {
            body.push_str(&statement);
            body.push('\n');
        }
    }
    let text = format!(
        "$timescale 1ns $end\n\
         $scope module tb $end\n\
         $scope module dut $end\n\
         $var wire 1 ! clk $end\n\
         $var wire 16 a in_data $end\n\
         $var wire 16 b out_data $end\n\
         $upscope $end\n\
         $upscope $end\n\
         $enddefinitions $end\n{body}"
    );
    Dump::open_vcd_bytes(text.into_bytes()).expect("reads")
}

fn clock_of(dump: &mut Dump, matches: &rtlscope_wave::MatchReport) -> Cycles {
    let clock = matches
        .matched
        .iter()
        .find(|found| found.ir_name.ends_with("clk"))
        .map(|found| found.ir_name.clone())
        .expect("the design has a clock the recording carries");
    cycles(dump, matches, &clock).expect("the clock has edges")
}

/// Three registers, three cycles, one bin — and the two layers agree, which is
/// the verdict that says the short road is the one being taken.
#[test]
fn a_pipeline_that_never_stalls_measures_what_the_structure_says() {
    let design = design("pipeline3.sv", Some("pipeline3"));
    let flat = flatten(&design);
    let mut dump = a_faithful_pipeline();
    let matches = match_signals(&dump, &design, &flat, None);
    let cycles = clock_of(&mut dump, &matches);

    let report =
        latency(&mut dump, &matches, &cycles, "in_data", "out_data").expect("both are there");

    assert_eq!(report.min, Some(3), "{report:#?}");
    assert_eq!(report.max, Some(3), "and never any longer");
    assert_eq!(report.median, Some(3));
    assert_eq!(report.histogram.len(), 1, "one distance: {:#?}", report.histogram);
    assert!(report.samples >= 10, "over enough beats to mean something: {}", report.samples);

    let structure = rtlscope_analyse::depth::analyse(&design, "in_data", "out_data");
    assert_eq!(structure.min_stages, Some(3), "the structure says the same");
    let verdict = rtlscope_wave::cross_check(&structure, &report);
    assert!(verdict.iter().any(|line| line.contains("actually takes")), "{verdict:?}");
}

/// A real run of the sample built for variable latency.
///
/// The interesting thing is what it turns out to be: the gate does not *delay*
/// beats, it **drops** them, so every beat that got through took the same time.
/// The structure calls the latency variable and it is right — but the variation
/// is in which beats arrive, not in how long they take, and only holding the
/// two layers together says so.
#[test]
fn a_gated_path_is_variable_in_which_beats_arrive_not_in_how_long_they_take() {
    let design = design("trace_demo.sv", Some("trace_demo"));
    let flat = flatten(&design);
    let mut dump = Dump::open(&rtlscope_fixtures::wave("trace_demo.vcd")).expect("opens");
    let matches = match_signals(&dump, &design, &flat, None);
    let cycles = clock_of(&mut dump, &matches);

    let report =
        latency(&mut dump, &matches, &cycles, "in_data", "out_data").expect("both are there");
    assert!(report.samples > 0, "{report:#?}");

    let structure = rtlscope_analyse::depth::analyse(&design, "in_data", "out_data");
    assert!(structure.variable_latency, "the structure calls it variable: {structure:#?}");

    let verdict = rtlscope_wave::cross_check(&structure, &report);
    assert!(!verdict.is_empty(), "and the two are held against each other: {verdict:?}");
}

/// A name the recording does not carry is answered with which one and why, not
/// with a number and a shrug.
#[test]
fn a_signal_the_recording_does_not_have_says_which_and_why() {
    let design = design("pipeline3.sv", Some("pipeline3"));
    let flat = flatten(&design);
    let mut dump = a_faithful_pipeline();
    let matches = match_signals(&dump, &design, &flat, None);
    let cycles = clock_of(&mut dump, &matches);

    let refused = latency(&mut dump, &matches, &cycles, "in_data", "valid_d2");
    let Err(why) = refused else { panic!("`valid_d2` is not in this recording") };
    let said = why.to_string();
    assert!(said.contains("valid_d2"), "it names the one that is missing: {said}");
}

/// What the report is for: the two numbers apart.
#[test]
fn a_stall_reads_as_a_floor_the_structure_does_not_explain() {
    let structure = rtlscope_analyse::DepthReport {
        from: "in_data".into(),
        to: "out_data".into(),
        clock: Some("clk".into()),
        min_stages: Some(3),
        max_stages: Some(3),
        feedback: false,
        reconvergent: false,
        variable_latency: true,
        paths: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        problems: Vec::new(),
    };
    let measured = LatencyReport {
        from: "in_data".into(),
        to: "out_data".into(),
        from_path: "tb.dut.in_data".into(),
        to_path: "tb.dut.out_data".into(),
        clock: "clk".into(),
        samples: 100,
        min: Some(7),
        median: Some(9),
        max: Some(21),
        histogram: Vec::new(),
        problems: Vec::new(),
    };

    let verdict = rtlscope_wave::cross_check(&structure, &measured);
    assert!(verdict[0].contains("never faster than 7"), "{verdict:?}");
    assert!(verdict.iter().any(|line| line.contains("every cycle")), "{verdict:?}");
    assert!(verdict.iter().any(|line| line.contains("stalls")), "{verdict:?}");
}

/// The sample's own recording, measured.
///
/// The interesting one is `stamped`: the structure says 3 **or** 4, and every
/// beat measures 3 — so the value arriving is the one escorted by the short
/// road, which is the bug the sample was built around. A single number from
/// either layer would have said nothing about it.
#[test]
fn the_sample_measures_the_road_the_qualifier_takes() {
    let design = design("depth_demo.sv", Some("depth_demo"));
    let flat = flatten(&design);
    let mut dump = Dump::open(&rtlscope_fixtures::wave("depth_demo.fst")).expect("opens");
    let matches = match_signals(&dump, &design, &flat, None);
    let cycles = clock_of(&mut dump, &matches);

    let report =
        latency(&mut dump, &matches, &cycles, "sample_in", "stamped").expect("both are there");
    assert!(report.samples > 100, "a long enough run: {}", report.samples);
    assert_eq!(report.histogram.len(), 1, "one distance: {:#?}", report.histogram);
    assert_eq!(report.min, Some(3), "{report:#?}");

    let structure = rtlscope_analyse::depth::analyse(&design, "sample_in", "stamped");
    assert_eq!((structure.min_stages, structure.max_stages), (Some(3), Some(4)));

    let verdict = rtlscope_wave::cross_check(&structure, &report);
    assert!(
        verdict.iter().any(|line| line.contains("actually takes")),
        "the short road is the one taken: {verdict:?}"
    );
}
