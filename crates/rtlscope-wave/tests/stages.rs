//! The pipeline's stages laid against a dump's cycles.
//!
//! The dumps are written rather than simulated, for the same reason the decoder
//! fixtures are: a simulator produces correct waveforms, and correct waveforms
//! exercise the easy half. The cases that decide whether this is trustworthy —
//! a stage that stalls, a register the dump never recorded, a cycle where the
//! valid bit is undriven — have to be made on purpose.

use rtlscope_analyse::{flat, pipeline};
use rtlscope_ir::Design;
use rtlscope_sv::ParseOptions;
use rtlscope_wave::stages::{Basis, Cell};
use rtlscope_wave::{Dump, match_signals};

/// The registers `pipeline3` has, minus the clock, which the builder drives.
const NETS: &[(&str, u32)] = &[
    ("rst_n", 1),
    ("in_valid", 1),
    ("in_data", 16),
    ("out_valid", 1),
    ("out_data", 16),
    ("valid_d1", 1),
    ("valid_d2", 1),
    ("valid_d3", 1),
    ("data_d1", 16),
    ("data_d2", 16),
    ("data_d3", 16),
];

fn design() -> Design {
    let path = rtlscope_fixtures::path("pipeline3.sv");
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, Some("pipeline3"));
    design.expect("elaboration produced a design")
}

fn identifier(index: usize) -> char {
    (b'!' + index as u8) as char
}

fn bits(value: u64, width: u32) -> String {
    (0..width).rev().map(|bit| if value >> bit & 1 == 1 { '1' } else { '0' }).collect()
}

/// A dump under `tb.dut`, one rising edge per row.
///
/// The values in a row are written at the edge's own timestamp, which is where
/// a simulator puts them: a register's value *during* a cycle is what it
/// settled on at the edge that began it.
fn vcd(nets: &[(&str, u32)], rows: &[Vec<(&str, String)>]) -> Dump {
    let mut text =
        String::from("$timescale 1ns $end\n$scope module tb $end\n$scope module dut $end\n");
    text.push_str("$var wire 1 ~ clk $end\n");
    for (index, (name, width)) in nets.iter().enumerate() {
        text.push_str(&format!("$var wire {width} {} {name} $end\n", identifier(index)));
    }
    text.push_str("$upscope $end\n$upscope $end\n$enddefinitions $end\n");

    text.push_str("#0\n0~\n");
    for (index, (_, width)) in nets.iter().enumerate() {
        text.push_str(&format!("b{} {}\n", "0".repeat(*width as usize), identifier(index)));
    }

    let id_of = |name: &str| {
        nets.iter()
            .position(|(known, _)| *known == name)
            .map(identifier)
            .unwrap_or_else(|| panic!("`{name}` is not one of the fixture's nets"))
    };

    let mut time = 0u64;
    for row in rows {
        time += 10;
        text.push_str(&format!("#{time}\n1~\n"));
        for (name, value) in row {
            text.push_str(&format!("b{value} {}\n", id_of(name)));
        }
        text.push_str(&format!("#{}\n0~\n", time + 5));
    }
    Dump::open_vcd_bytes(text.into_bytes()).expect("the fixture parses")
}

/// Runs `pipeline3`'s own arithmetic over an input pattern, so the fixture is
/// the design's behaviour rather than a guess at it.
fn burst(pattern: &[bool], nets: &[(&str, u32)]) -> Vec<Vec<(&'static str, String)>> {
    let mut rows = Vec::new();
    let mut valid = [false; 3];
    let mut data = [0u64; 3];
    let has = |name: &str| nets.iter().any(|(known, _)| *known == name);

    for (beat, feed) in pattern.iter().enumerate() {
        // The registers shift, deepest first so each takes the old value.
        data[2] = data[1] ^ 0xFFFF;
        data[1] = (data[0] + 1) & 0xFFFF;
        data[0] = beat as u64 & 0xFFFF;
        valid[2] = valid[1];
        valid[1] = valid[0];
        valid[0] = *feed;

        let mut row: Vec<(&'static str, String)> = vec![
            ("rst_n", "1".to_string()),
            ("in_valid", u8::from(*feed).to_string()),
            ("in_data", bits(beat as u64, 16)),
        ];
        for (index, name) in ["valid_d1", "valid_d2", "valid_d3"].iter().enumerate() {
            if has(name) {
                row.push((name, u8::from(valid[index]).to_string()));
            }
        }
        for (index, name) in ["data_d1", "data_d2", "data_d3"].iter().enumerate() {
            if has(name) {
                row.push((name, bits(data[index], 16)));
            }
        }
        rows.push(row);
    }
    rows
}

/// Lays the design's only clock domain against a dump.
fn lay_out(dump: Dump, first: usize, len: usize) -> rtlscope_wave::StageView {
    lay_out_with(dump, &rtlscope_wave::Layout::window(first, len))
}

fn lay_out_with(mut dump: Dump, layout: &rtlscope_wave::Layout) -> rtlscope_wave::StageView {
    let design = design();
    let flat = flat::flatten(&design);
    let matches = match_signals(&dump, &design, &flat, None);
    let report = pipeline::analyse(&design);
    let domain = report.domains.first().expect("pipeline3 has a clock domain");

    let cycles = rtlscope_wave::stages::cycles(&mut dump, &matches, &domain.clock)
        .expect("the clock is in the dump");
    rtlscope_wave::stages::occupancy(&mut dump, &matches, domain, &cycles, layout)
}

fn glyphs(view: &rtlscope_wave::StageView, stage: usize) -> String {
    view.rows
        .iter()
        .find(|row| row.stage == stage)
        .map(|row| row.cells.iter().map(|cell| cell.glyph()).collect())
        .unwrap_or_else(|| panic!("no row for stage {stage}"))
}

/// The picture a pipeline is always explained with: each stage lit one cycle
/// after the one above it.
#[test]
fn each_stage_lights_one_cycle_after_the_one_above_it() {
    let pattern: Vec<bool> = (0..24).map(|beat| beat % 8 < 4).collect();
    let view = lay_out(vcd(NETS, &burst(&pattern, NETS)), 0, 24);

    assert_eq!(view.clock, "clk");
    assert_eq!(view.clock_path, "tb.dut.clk");
    assert_eq!(view.depth, 3);
    assert_eq!(view.rows.len(), 3);

    // Cycle 0 is the first edge, at which `valid_d1` takes `in_valid`.
    assert_eq!(glyphs(&view, 0), "####....####....####....");
    assert_eq!(glyphs(&view, 1), ".####....####....####...");
    assert_eq!(glyphs(&view, 2), "..####....####....####..");
    assert!(view.problems.is_empty(), "{:?}", view.problems);
}

/// Which signal decided a row is part of the answer, not a footnote: a row read
/// from a valid bit and one read from movement mean different things.
#[test]
fn every_row_names_the_valid_bit_it_read() {
    let pattern: Vec<bool> = (0..8).map(|beat| beat % 4 == 0).collect();
    let view = lay_out(vcd(NETS, &burst(&pattern, NETS)), 0, 8);

    for (index, row) in view.rows.iter().enumerate() {
        let wanted = format!("valid_d{}", index + 1);
        assert_eq!(row.basis, Basis::Valid { signal: wanted.clone() }, "stage {index}");
        // The payload is the widest register that is not the valid.
        assert_eq!(row.payload.as_deref(), Some(format!("data_d{}", index + 1).as_str()));
    }
}

/// A stage carrying the same thing two cycles running has not advanced, and
/// saying so is most of what a pipeline diagram is read for.
#[test]
fn a_stage_that_repeats_while_valid_stays_high_reads_as_a_stall() {
    // Four cycles of valid, and `data_d1` frozen through the middle two.
    let mut rows: Vec<Vec<(&'static str, String)>> = Vec::new();
    for cycle in 0..6u64 {
        let held = (2..=3).contains(&cycle);
        rows.push(vec![
            ("rst_n", "1".to_string()),
            ("in_valid", "1".to_string()),
            ("in_data", bits(cycle, 16)),
            ("valid_d1", if (1..=4).contains(&cycle) { "1" } else { "0" }.to_string()),
            ("valid_d2", "0".to_string()),
            ("valid_d3", "0".to_string()),
            ("data_d1", bits(if held { 7 } else { cycle }, 16)),
            ("data_d2", bits(0, 16)),
            ("data_d3", bits(0, 16)),
        ]);
    }
    let view = lay_out(vcd(NETS, &rows), 0, 6);

    // Cycle 3 repeats what cycle 2 held while the valid stayed up: a stall.
    assert_eq!(glyphs(&view, 0), ".##~#.");
    let row = &view.rows[0];
    assert_eq!(row.held, 1);
    assert_eq!(row.occupied, 4);
}

/// A row the dump cannot answer for is blank and says why, rather than reading
/// as an empty stage — which would be a claim about the design.
#[test]
fn a_stage_the_dump_does_not_have_is_blank_and_says_so() {
    let without: Vec<(&str, u32)> = NETS
        .iter()
        .copied()
        .filter(|(name, _)| !name.starts_with("valid_d2") && *name != "data_d2")
        .collect();
    let pattern: Vec<bool> = (0..8).map(|beat| beat % 4 < 2).collect();
    let view = lay_out(vcd(&without, &burst(&pattern, &without)), 0, 8);

    assert_eq!(view.rows[1].basis, Basis::Absent);
    assert_eq!(view.rows[1].matched, 0);
    assert!(view.rows[1].cells.iter().all(|cell| *cell == Cell::Blank));
    assert!(view.problems.iter().any(|problem| problem.contains("stage 1")), "{:?}", view.problems);
    // The stages either side still read normally.
    assert_eq!(glyphs(&view, 0), "##..##..");
}

/// An undriven valid is not a zero, and a stage that read it as one would show
/// an empty pipeline where there is really no answer.
#[test]
fn an_undriven_valid_is_marked_rather_than_read_as_empty() {
    let pattern: Vec<bool> = vec![true; 6];
    let mut rows = burst(&pattern, NETS);
    for (name, value) in rows[3].iter_mut() {
        if *name == "valid_d1" {
            *value = "x".to_string();
        }
    }
    let view = lay_out(vcd(NETS, &rows), 0, 6);
    assert_eq!(glyphs(&view, 0), "###?##");
}

/// A real design calls its valid `v3` as often as `d3_valid`, and no heuristic
/// should guess at the first. So a caller can name it, and the name wins.
#[test]
fn a_valid_named_by_the_caller_beats_the_one_guessed_at() {
    let pattern: Vec<bool> = (0..8).map(|beat| beat % 4 < 2).collect();
    let dump = vcd(NETS, &burst(&pattern, NETS));

    let mut layout = rtlscope_wave::Layout::window(0, 8);
    layout.valid = vec![(0, "valid_d3".to_string())];
    layout.payload = vec![(0, "data_d3".to_string())];
    let view = lay_out_with(dump, &layout);

    assert_eq!(view.rows[0].basis, Basis::Valid { signal: "valid_d3".to_string() });
    assert_eq!(view.rows[0].payload.as_deref(), Some("data_d3"));
    // Stage 2's picture, on stage 0's row, because that is what was asked for.
    assert_eq!(glyphs(&view, 0), "..##..##");
    // Every other row is still guessed at.
    assert_eq!(view.rows[1].basis, Basis::Valid { signal: "valid_d2".to_string() });
}

/// A name that does not resolve falls back — and says it fell back, rather than
/// leaving a row that looks chosen and was not.
#[test]
fn a_named_signal_that_cannot_be_used_is_reported() {
    let pattern: Vec<bool> = vec![true; 4];
    let dump = vcd(NETS, &burst(&pattern, NETS));

    let mut layout = rtlscope_wave::Layout::window(0, 4);
    // One that is not there at all, and one that is the wrong shape for a valid.
    layout.valid = vec![(0, "no_such_signal".to_string()), (1, "data_d2".to_string())];
    layout.payload = vec![(2, "also_missing".to_string())];
    let view = lay_out_with(dump, &layout);

    assert!(
        view.problems.iter().any(|p| p.contains("`no_such_signal` is not in the dump")),
        "{:?}",
        view.problems
    );
    assert!(
        view.problems.iter().any(|p| p.contains("`data_d2` is 16 bits")),
        "{:?}",
        view.problems
    );
    assert!(
        view.problems.iter().any(|p| p.contains("`also_missing` is not in the dump")),
        "{:?}",
        view.problems
    );
    // And each row went back to what it would have shown anyway.
    assert_eq!(view.rows[0].basis, Basis::Valid { signal: "valid_d1".to_string() });
    assert_eq!(view.rows[1].basis, Basis::Valid { signal: "valid_d2".to_string() });
    assert_eq!(view.rows[2].payload.as_deref(), Some("data_d3"));
}

/// A window past the end of the dump is trimmed and said to have been, rather
/// than coming back short without explanation.
#[test]
fn asking_for_more_cycles_than_there_are_says_so() {
    let pattern: Vec<bool> = vec![true; 5];
    let view = lay_out(vcd(NETS, &burst(&pattern, NETS)), 0, 500);

    assert_eq!(view.cycles, 5);
    assert_eq!(view.len(), 5);
    assert!(
        view.problems.iter().any(|problem| problem.contains("500 cycle(s) were asked for")),
        "{:?}",
        view.problems
    );
}
