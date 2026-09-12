//! Spans must name the line the author wrote, even when an `include` sits
//! between it and the top of the file.
//!
//! This is the regression test for the one measurement that shaped the front
//! end: `Locate::line` counts preprocessed lines, so on this fixture it reports
//! 7 for a `module` keyword that is on line 4. Anything built on `Locate::line`
//! would send "jump to source" three lines off for every file with an include.

use std::path::PathBuf;

use rtlscope_sv::{ParseOptions, SourceMap, parse_file};
use sv_parser::{NodeEvent, RefNode};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// Every `SimpleIdentifier` in the file, as `(text, resolved span)`.
fn identifier_spans() -> (Vec<(String, rtlscope_ir::Span)>, rtlscope_ir::FileTable) {
    let path = data_dir().join("with_include.sv");
    let options = ParseOptions { defines: Vec::new(), include_paths: vec![data_dir()] };
    let parsed = parse_file(&path, &options).expect("with_include.sv parses");

    let mut map = SourceMap::new();
    let mut out = Vec::new();
    for event in parsed.tree.into_iter().event() {
        let NodeEvent::Enter(RefNode::SimpleIdentifier(node)) = event else { continue };
        let locate = node.nodes.0;
        let text = parsed.tree.get_str(&locate).unwrap_or_default().to_string();
        out.push((text, map.resolve(&parsed.tree, &locate)));
    }
    (out, map.into_files())
}

fn find<'a>(
    spans: &'a [(String, rtlscope_ir::Span)],
    name: &str,
) -> &'a (String, rtlscope_ir::Span) {
    spans.iter().find(|(text, _)| text == name).unwrap_or_else(|| panic!("no identifier {name}"))
}

#[test]
fn spans_point_at_the_original_line_not_the_preprocessed_one() {
    let (spans, files) = identifier_spans();

    // `module with_include (` is line 4 of the file as written. sv-parser's
    // Locate reports line 7 here, because the include contributed three lines.
    let (_, span) = find(&spans, "with_include");
    assert_eq!(span.line, 4, "module identifier should be on line 4 as written");
    assert_eq!(span.col, 8, "column 8 is just past `module `");

    let path = files.path(span.file).expect("span resolves to a file");
    assert!(path.ends_with("with_include.sv"), "{}", path.display());
}

#[test]
fn tokens_from_an_included_file_point_into_that_file() {
    let (spans, files) = identifier_spans();

    let (_, span) = find(&spans, "DATA_W");
    let path = files.path(span.file).expect("span resolves to a file");
    assert!(path.ends_with("defs.svh"), "macro name lives in the header: {}", path.display());
    assert_eq!(span.line, 3, "`define DATA_W is on line 3 of defs.svh");
}

#[test]
fn ports_declared_after_the_include_keep_their_own_lines() {
    let (spans, files) = identifier_spans();

    let (_, din) = find(&spans, "din");
    assert_eq!(din.line, 5);
    let (_, dout) = find(&spans, "dout");
    assert_eq!(dout.line, 6);

    assert!(files.path(din.file).unwrap().ends_with("with_include.sv"));
}

#[test]
fn no_span_is_left_unknown() {
    let (spans, _) = identifier_spans();
    assert!(!spans.is_empty());
    for (text, span) in &spans {
        assert!(!span.is_unknown(), "identifier {text} resolved to no location");
    }
}
