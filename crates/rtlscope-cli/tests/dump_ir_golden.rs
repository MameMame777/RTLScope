//! Golden tests for `rtlscope dump-ir`.
//!
//! Where the `dump-ports` goldens pin what was *parsed*, these pin what was
//! *elaborated*: the widths, the specialised module names, and the resolved
//! connections. A change to constant folding or generate unrolling shows up
//! here as a diff rather than as a wrong diagram three steps later.

use rtlscope_cli::cmd::dump_ir;
use rtlscope_sv::ParseOptions;

fn report(fixture: &str, top: Option<&str>) -> serde_json::Value {
    let path = rtlscope_fixtures::path(fixture);
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, top);
    let design = design.expect("elaboration produced a design");

    let mut value = serde_json::to_value(dump_ir::build(&design)).expect("serialises");
    strip_fixture_dir(&mut value, &fixture_dir_prefixes());
    value
}

fn fixture_dir_prefixes() -> Vec<String> {
    let dir = dunce::canonicalize(rtlscope_fixtures::dir()).expect("fixture directory");
    let dir = dir.display().to_string();
    let dir = dir.trim_end_matches(['/', '\\']).to_string();
    vec![format!("{dir}\\"), format!("{dir}/")]
}

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
fn hierarchy_with_resolved_widths_and_connections() {
    insta::assert_json_snapshot!(report("hier.sv", Some("hier_top")));
}

#[test]
fn parameter_specialisation_splits_one_module_into_two() {
    insta::assert_json_snapshot!(report("params.sv", Some("params_top")));
}

#[test]
fn clog2_and_a_memory_array() {
    insta::assert_json_snapshot!(report("fifo.sv", None));
}

#[test]
fn generate_unrolled() {
    insta::assert_json_snapshot!(report("genblk.sv", None));
}

/// Functions, loops, compound assignment and the constant folding they need.
///
/// The fixture is deliberately dense: every construct in it was, at some point,
/// dropped without a trace, and the shape of the inlined nets — one set per call
/// site — is the part most likely to drift.
#[test]
fn functions_inlined_at_each_call_site() {
    insta::assert_json_snapshot!(report("function.sv", Some("function_test")));
}

/// Tasks, signal-bounded loops, `initial` and an early `return`.
///
/// The interesting part of the snapshot is what is *not* in it: no
/// `Unsupported` holes. Each of these constructs looks like control flow and
/// turns out to be ordinary logic, and the shape it turns into — guarded
/// copies, a copy-back, a flag — is what would drift if the reasoning changed.
#[test]
fn control_flow_that_turns_out_to_be_logic() {
    insta::assert_json_snapshot!(report("task_loop.sv", Some("task_loop")));
}
