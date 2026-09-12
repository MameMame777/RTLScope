//! What the front end says about the constructs it cannot model.
//!
//! D2 makes this a correctness property, not a nicety: a design RTLScope only
//! half understood must say so. These tests pin the cases where the lowering
//! has to keep going with a placeholder — the placeholder is allowed, staying
//! quiet about it is not.

use rtlscope_ir::{DiagCode, Diagnostics};
use rtlscope_sv::ParseOptions;

fn diagnose(fixture: &str) -> (Diagnostics, rtlscope_ir::FileTable) {
    let path = rtlscope_fixtures::path(fixture);
    let (design, diags) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    (diags, design.files)
}

fn find(diags: &Diagnostics, code: DiagCode) -> Vec<(u32, String)> {
    diags
        .iter()
        .filter(|d| d.code == code)
        .map(|d| (d.span.map_or(0, |s| s.line), d.message.clone()))
        .collect()
}

#[test]
fn a_type_with_no_typedef_in_reach_is_reported_rather_than_assumed_to_be_one_bit() {
    // A name RTLScope cannot size — from a package, or a struct — is modelled as
    // one bit, and says so. Guessing silently is the wrongness D2 exists to
    // prevent. (An enum declared in the same module is now resolved properly;
    // see the elaboration tests.)
    let dir = std::env::temp_dir().join("rtlscope-unknown-type");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("unknown_type.sv");
    std::fs::write(
        &path,
        "module unknown_type (input logic clk);
    from_a_package_t x;
endmodule
",
    )
    .expect("writing the generated source");

    let (uir, mut diags) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (_, elab) = rtlscope_elab::elaborate(&uir, None);
    diags.extend(elab);

    let found = find(&diags, DiagCode::UnknownDataType);
    assert_eq!(found.len(), 1, "expected one unknown-type report, got {found:?}");
    assert!(found[0].1.contains("from_a_package_t"), "the report names it: {}", found[0].1);
}

#[test]
fn defparam_is_an_error_not_a_skip() {
    let (diags, _) = diagnose("unsupported.sv");
    let found = find(&diags, DiagCode::DefparamUnsupported);

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, 56);
    assert!(diags.has_errors(), "defparam must fail the run, not just warn");
}

#[test]
fn skipped_constructs_name_the_keyword_the_author_wrote() {
    let (diags, _) = diagnose("unsupported.sv");
    let messages: Vec<String> =
        find(&diags, DiagCode::UnsupportedConstruct).into_iter().map(|(_, m)| m).collect();

    for keyword in ["forever", "#", "casex"] {
        assert!(
            messages.iter().any(|m| m.contains(&format!("`{keyword}`"))),
            "no report mentions `{keyword}`: {messages:?}"
        );
    }
}

#[test]
fn a_design_within_the_subset_is_understood_completely() {
    // hier.sv exercises every connection style, three levels of hierarchy,
    // flops, combinational logic and a memory. Nothing in it is outside the
    // subset, so the front end should have nothing at all to say.
    for fixture in ["hier.sv", "fifo.sv", "counter.sv", "fsm.sv", "pipeline3.sv", "genblk.sv"] {
        let (diags, files) = diagnose(fixture);
        assert!(
            diags.is_empty(),
            "{fixture} should lower cleanly but reported:
{}",
            diags.render(&files)
        );
    }
}

#[test]
fn a_syntax_error_names_the_line_not_a_byte_offset() {
    // sv-parser reports the location as a bare `Option<(PathBuf, usize)>`, which
    // prints as a Rust tuple if passed through untranslated.
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/broken.sv");
    let (design, diags) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());

    assert!(diags.has_errors());
    let rendered = diags.render(&design.files);
    assert!(rendered.contains("broken.sv:5:14"), "expected a line:col, got:\n{rendered}");
    assert!(!rendered.contains("Some(("), "raw error tuple leaked into the message:\n{rendered}");
}

#[test]
fn one_unparseable_file_does_not_take_the_others_with_it() {
    let broken = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/broken.sv");
    let good = rtlscope_fixtures::path("adder.sv");
    let (design, diags) = rtlscope_sv::lower_files(&[broken, good], &ParseOptions::default());

    assert!(diags.has_errors(), "the broken file is still reported");
    assert!(
        design.module_by_name("adder").is_some(),
        "the good file should still have been lowered"
    );
}

#[test]
fn every_diagnostic_can_be_pointed_at_a_line() {
    // A report with no location is one the user cannot act on.
    for fixture in rtlscope_fixtures::NAMES {
        let (diags, files) = diagnose(fixture);
        for diag in diags.iter() {
            let Some(span) = diag.span else {
                panic!("{fixture}: diagnostic with no span: {}", diag.message);
            };
            assert!(
                !span.is_unknown() && files.path(span.file).is_some(),
                "{fixture}: unresolvable span on: {}",
                diag.message
            );
        }
    }
}

#[test]
fn a_deeply_nested_expression_does_not_blow_the_stack() {
    // `sv-parser` recurses with the expression nesting, and the main thread's
    // stack is not enough for real RTL — a real 41-file design had 25 of its
    // files abort the process outright. On an 8 MB thread those files parse but
    // 800 chained `+` terms still overflow, so this pins something well past
    // that: if the big stack is ever removed, this test crashes the run rather
    // than quietly passing.
    //
    // 1200 terms: half again past the 800 that overflows 8 MB, and about three
    // seconds to parse. Deeper would be a better margin but the parse time
    // climbs steeply — 4000 terms takes forty.
    //
    // Generated rather than committed: it is noise whose only interesting
    // property is its depth.
    let terms = vec!["a"; 1200].join(" + ");
    let source = format!(
        "module deep (input logic [7:0] a, output logic [7:0] y);\n  assign y = {terms};\nendmodule\n"
    );

    let dir = std::env::temp_dir().join("rtlscope-deep-nesting");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("deep.sv");
    std::fs::write(&path, source).expect("writing the generated source");

    let (design, diags) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    assert!(
        design.module_by_name("deep").is_some(),
        "the deep expression should parse: {}",
        diags.render(&design.files)
    );
}

#[test]
fn a_process_that_declares_a_variable_inside_itself_is_still_a_process() {
    // Regression, found on real RTL. Items are classified by searching them for
    // the construct they contain, and a declaration nested inside an `always`
    // block matched first — so the block was read as a net declaration, the
    // process vanished, and nothing was reported because the lowering believed
    // it had understood the item. Whole flops disappeared from real modules.
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/decl_in_process.sv");
    let (uir, diags) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());

    let module = uir.module_by_name("decl_in_process").expect("the module");
    let processes =
        module.items.iter().filter(|item| matches!(item, rtlscope_ir::UItem::Proc { .. })).count();

    assert_eq!(processes, 1, "the always_ff block must survive");
    assert!(diags.is_empty(), "and cleanly: {}", diags.render(&uir.files));
}
