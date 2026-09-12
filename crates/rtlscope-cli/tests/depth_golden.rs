//! What the `depth` command prints, held to the page.
//!
//! The snapshot is taken of the renderer rather than of the binary, like the
//! other golden tests here: what is being pinned is the wording a reader gets,
//! and running a process to find it out would test the argument parser at the
//! same time and fail for two reasons at once.

use rtlscope_analyse::depth;
use rtlscope_ir::Design;
use rtlscope_sv::ParseOptions;

fn design(fixture: &str, top: Option<&str>) -> Design {
    let path = rtlscope_fixtures::path(fixture);
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    rtlscope_elab::elaborate(&uir, top).0.expect("elaborates")
}

/// Spans are rendered with an absolute path, which differs by machine.
///
/// Cut back to the fixture's own name. Done by finding the directory inside
/// the rendered path rather than by matching a prefix built here: the two are
/// the same directory but not always the same string, and a drive letter's
/// case is enough to make one not contain the other.
///
/// Only where the directory is actually there — one of the sentences reads
/// "at least 1 clock(s)", and treating that as a path made a snapshot claiming
/// a file called `least`.
fn steady(text: &str) -> String {
    let ended = text.ends_with('\n');
    let mut out: String = text
        .replace('\\', "/")
        .lines()
        .map(|line| match line.split_once("tests/fixtures/") {
            Some((head, tail)) => {
                let head = head.trim_end_matches(|c: char| !c.is_whitespace());
                format!("{head}<fixtures>/{tail}\n")
            }
            None => format!("{line}\n"),
        })
        .collect();
    if !ended {
        out.pop();
    }
    out
}

fn rendered(fixture: &str, top: Option<&str>, from: &str, to: &str) -> String {
    let design = design(fixture, top);
    let report = depth::analyse(&design, from, to);
    steady(&rtlscope_cli::cmd::analyse::depth_text(&report, &design.files))
}

/// The plain answer: one road, one number, and the registers it runs through.
#[test]
fn a_straight_pipeline_is_a_number_and_the_registers_it_counts() {
    insta::assert_snapshot!(rendered("pipeline3.sv", Some("pipeline3"), "in_data", "out_data"));
}

/// Two roads of different length, both reported rather than averaged.
#[test]
fn two_roads_of_different_length_show_both_and_say_why_that_matters() {
    insta::assert_snapshot!(rendered("reconverge.sv", Some("reconverge"), "a", "sum"));
}

/// A crossing has no answer, and the message says where to ask instead.
#[test]
fn a_crossing_refuses_and_points_at_the_command_that_reports_it() {
    insta::assert_snapshot!(rendered("cdc.sv", Some("cdc_top"), "pulse", "flag_out"));
}

/// A value that decides itself never settles, so there is nothing to count.
#[test]
fn a_cycle_of_wires_refuses_the_count() {
    insta::assert_snapshot!(rendered("comb_loop.sv", Some("comb_loop"), "seed", "out_knot"));
}

/// A gate does not change the depth, only when it is paid — and the two facts
/// are stated apart.
#[test]
fn a_gated_register_is_this_deep_and_not_this_often() {
    insta::assert_snapshot!(rendered("trace_demo.sv", Some("trace_demo"), "in_data", "out_data"));
}

/// A counter on the road: a floor, no ceiling, and the reason in words.
#[test]
fn a_road_that_loops_gives_a_floor_and_says_why_there_is_no_ceiling() {
    insta::assert_snapshot!(rendered("counter.sv", None, "en", "count"));
}

/// A name the design does not have is answered with names it does.
#[test]
fn an_unknown_name_offers_the_ones_that_exist() {
    insta::assert_snapshot!(rendered("pipeline3.sv", Some("pipeline3"), "in_dat", "out_data"));
}
