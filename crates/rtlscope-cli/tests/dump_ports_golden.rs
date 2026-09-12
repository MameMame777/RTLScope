//! Golden tests for `rtlscope dump-ports`.
//!
//! These pin what the front end understood about each fixture. When a lowering
//! change is intentional, `cargo insta review` shows exactly what moved; when it
//! is not, the diff is the bug report.
//!
//! Absolute paths are stripped before snapshotting. They differ per machine,
//! and a snapshot full of `E:\Nautilus\...` would fail for everyone else.

use rtlscope_cli::cmd::dump_ports;
use rtlscope_sv::ParseOptions;

/// Lowers one fixture and returns the report with machine-specific paths removed.
fn report(fixture: &str) -> serde_json::Value {
    let path = rtlscope_fixtures::path(fixture);
    let (design, _diags) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let report = dump_ports::build(&design);

    let mut value = serde_json::to_value(&report).expect("report serialises");
    strip_fixture_dir(&mut value, &fixture_dir_prefixes());
    value
}

/// The fixture directory as it appears in the report, with both separators.
///
/// The report canonicalises through `dunce`, so this has to as well or the
/// prefix will not match.
fn fixture_dir_prefixes() -> Vec<String> {
    let dir = dunce::canonicalize(rtlscope_fixtures::dir()).expect("fixture directory");
    let dir = dir.display().to_string();
    let dir = dir.trim_end_matches(['/', '\\']).to_string();
    vec![format!("{dir}\\"), format!("{dir}/")]
}

/// Removes the fixture directory wherever it appears, leaving the file name and
/// anything written around it — `"always at <dir>/x.sv:13:5"` becomes
/// `"always at x.sv:13:5"`, so a golden test still shows *what* was skipped.
fn strip_fixture_dir(value: &mut serde_json::Value, prefixes: &[String]) {
    match value {
        serde_json::Value::String(text) => {
            for prefix in prefixes {
                *text = text.replace(prefix.as_str(), "");
            }
        }
        serde_json::Value::Array(items) => {
            items.iter_mut().for_each(|item| strip_fixture_dir(item, prefixes))
        }
        serde_json::Value::Object(map) => {
            map.values_mut().for_each(|item| strip_fixture_dir(item, prefixes))
        }
        _ => {}
    }
}

#[test]
fn hierarchy_with_all_three_connection_styles() {
    insta::assert_json_snapshot!(report("hier.sv"));
}

#[test]
fn parameterised_fifo_with_a_memory() {
    insta::assert_json_snapshot!(report("fifo.sv"));
}

#[test]
fn parameter_propagation() {
    insta::assert_json_snapshot!(report("params.sv"));
}

#[test]
fn non_ansi_header_takes_directions_from_the_body() {
    insta::assert_json_snapshot!(report("nonansi.sv"));
}

#[test]
fn constructs_outside_the_subset_are_reported_not_dropped() {
    insta::assert_json_snapshot!(report("unsupported.sv"));
}
