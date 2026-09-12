//! A file a tool generated is read, but its spans land in the file the author
//! wrote.
//!
//! The fixture is a Veryl project built with `veryl build`: `target/*.sv` and
//! the `.sv.map` beside each, and `src/*.veryl` they were made from. Nothing
//! here runs Veryl — the point is that the front end finds the map on its own.

use std::path::{Path, PathBuf};

use rtlscope_ir::{FileTable, Span, UDesign};
use rtlscope_sv::{ParseOptions, lower_files};

fn project() -> PathBuf {
    rtlscope_fixtures::veryl_project()
}

fn lowered(paths: &[PathBuf]) -> UDesign {
    let (design, diags) = lower_files(paths, &ParseOptions::default());
    assert_eq!(diags.count(rtlscope_ir::Severity::Error), 0, "{}", diags.render(&design.files));
    design
}

fn file_of(files: &FileTable, span: Span) -> &Path {
    files.path(span.file).expect("a real file")
}

/// The module Veryl wrote as `lights_Control` was declared on line 3 of the
/// `.veryl`, and that is where its span goes. A module's span starts at the
/// `module` keyword, in a generated file as in any other.
#[test]
fn a_module_written_in_veryl_is_placed_in_the_veryl() {
    let design = lowered(&[project().join("target").join("control.sv")]);
    let module = design.modules.iter().find(|m| m.name == "lights_Control").expect("lowered");

    let at = module.span;
    assert!(file_of(&design.files, at).ends_with("control.veryl"), "{}", design.files.render(at));
    assert_eq!((at.line, at.col), (3, 1), "`module Control (` — {}", design.files.render(at));
    assert_eq!(at.len, "module".len() as u32);
}

/// Ports keep their own lines even when Veryl renamed them: `i_rst` became
/// `i_rst_n` in the SystemVerilog, and the span still says `i_rst`, line 5.
#[test]
fn ports_point_at_their_declarations_in_the_veryl() {
    let design = lowered(&[project().join("target").join("control.sv")]);
    let module = design.modules.iter().find(|m| m.name == "lights_Control").expect("lowered");

    let port = |name: &str| module.ports.iter().find(|p| p.name == name).expect(name).span;
    let start = port("i_start");
    assert!(file_of(&design.files, start).ends_with("control.veryl"));
    assert_eq!((start.line, start.col), (6, 5), "{}", design.files.render(start));

    let reset = port("i_rst_n");
    assert_eq!((reset.line, reset.col), (5, 5), "{}", design.files.render(reset));
    assert_eq!(reset.len, "i_rst".len() as u32, "the name as written, not as generated");
}

/// The whole project, through the filelist Veryl wrote: every module comes
/// from a `.veryl`, and the generated files are in the table only because
/// they were read.
#[test]
fn every_module_of_the_project_is_placed_in_a_veryl_file() {
    let list = std::fs::read_to_string(project().join("lights.f")).expect("the filelist");
    let paths: Vec<PathBuf> = list.lines().map(|line| project().join(line.trim())).collect();
    let design = lowered(&paths);

    assert_eq!(
        design.modules.len(),
        4,
        "{:?}",
        design.modules.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    for module in &design.modules {
        let path = file_of(&design.files, module.span);
        assert!(
            path.extension().is_some_and(|ext| ext == "veryl"),
            "{} is placed in {}",
            module.name,
            path.display()
        );
        for port in &module.ports {
            assert!(
                file_of(&design.files, port.span).extension().is_some_and(|ext| ext == "veryl")
            );
        }
    }
}

/// A map that points at a file that is not there is a map to nowhere; the
/// generated file is the better of the two places left.
#[test]
fn a_generated_file_whose_original_is_gone_keeps_its_own_spans() {
    let dir = std::env::temp_dir().join("rtlscope-sv-orphan-map");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("target")).unwrap();
    for name in ["control.sv", "control.sv.map"] {
        std::fs::copy(project().join("target").join(name), dir.join("target").join(name)).unwrap();
    }
    // No src/control.veryl beside it.

    let design = lowered(&[dir.join("target").join("control.sv")]);
    let module = design.modules.iter().find(|m| m.name == "lights_Control").expect("lowered");
    let at = module.span;
    assert!(file_of(&design.files, at).ends_with("control.sv"), "{}", design.files.render(at));
    assert_eq!((at.line, at.col), (3, 1), "the generated file's own line");
}

/// Plain SystemVerilog is untouched by any of this.
#[test]
fn a_file_with_no_map_is_read_as_before() {
    let path = rtlscope_fixtures::path("hier.sv");
    let design = lowered(std::slice::from_ref(&path));
    for module in &design.modules {
        assert!(file_of(&design.files, module.span).ends_with("hier.sv"));
    }
}

/// Every name Veryl rewrote comes back as the author wrote it, beside the
/// name RTLScope read — and only where the two differ.
#[test]
fn names_come_back_as_the_author_wrote_them() {
    let design = lowered(&[project().join("target").join("control.sv")]);
    let module = design.modules.iter().find(|m| m.name == "lights_Control").expect("lowered");
    assert_eq!(module.written.as_deref(), Some("Control"), "the project prefix comes off");

    let port = |name: &str| module.ports.iter().find(|p| p.name == name).expect(name);
    assert_eq!(port("i_rst_n").written.as_deref(), Some("i_rst"), "the reset suffix comes off");
    assert_eq!(port("i_start").written, None, "a name that was not rewritten is not repeated");

    let members: Vec<(&str, Option<&str>)> = module
        .items
        .iter()
        .find_map(|item| match item {
            rtlscope_ir::UItem::TypedefEnum { members, .. } => Some(members),
            _ => None,
        })
        .expect("the state enum")
        .iter()
        .map(|m| (m.name.as_str(), m.written.as_deref()))
        .collect();
    assert_eq!(members[0], ("State_Idle", Some("Idle")), "{members:?}");

    // And the whole design answers to either name.
    let (elaborated, _) = rtlscope_elab::elaborate(&design, Some("Control"));
    let elaborated = elaborated.expect("--top by the written name");
    assert_eq!(elaborated.top_module().name, "lights_Control");
    assert_eq!(elaborated.top_module().shown(), "Control");
    assert!(elaborated.module_by_name("Control").is_some());
    assert!(elaborated.module_by_name("lights_Control").is_some());
}

/// A file the author wrote has no second name to know.
#[test]
fn plain_systemverilog_has_no_written_names() {
    let design = lowered(&[rtlscope_fixtures::path("fsm.sv")]);
    for module in &design.modules {
        assert_eq!(module.written, None);
        assert!(module.ports.iter().all(|p| p.written.is_none()));
    }
}

/// The design remembers which file was written from which, and the map can be
/// read in both directions, so a reader can be shown the generated file at
/// the line they were looking at.
#[test]
fn the_generated_file_and_its_original_are_paired_and_walkable() {
    let design = lowered(&[project().join("target").join("control.sv")]);
    assert_eq!(design.generated.len(), 1, "{:?}", design.generated);
    let pair = &design.generated[0];
    assert!(file_of(&design.files, Span::new(pair.generated, 1, 1, 1)).ends_with("control.sv"));
    assert!(file_of(&design.files, Span::new(pair.original, 1, 1, 1)).ends_with("control.veryl"));
    assert!(pair.map.ends_with("control.sv.map"), "{}", pair.map.display());

    // `module Control (` is line 3 of the .veryl and `module lights_Control (`
    // is line 3 of the .sv, both from column 1.
    assert_eq!(rtlscope_sv::generated_position(&pair.map, 3), Some((3, 1)));
    // `i_start` on line 6 of both: column 5 in the .veryl, and wherever
    // Veryl's alignment put it in the .sv — read from the file, since a port
    // added to the module moves it.
    let generated =
        std::fs::read_to_string(project().join("target").join("control.sv")).expect("control.sv");
    let sv_line = generated.lines().nth(5).expect("line 6");
    let sv_col = sv_line.find("i_start").expect("i_start on line 6") as u32 + 1;
    let (path, line, col) = rtlscope_sv::original_position(&pair.map, 6, sv_col).expect("mapped");
    assert!(path.ends_with("control.veryl"));
    assert_eq!((line, col), (6, 5));
    // A line the map does not speak for.
    assert_eq!(rtlscope_sv::generated_position(&pair.map, 9999), None);
}

/// A package Veryl wrote is read like the modules: its name the author's, its
/// constants and function reachable from the modules that import it, and the
/// `import` line itself placed in the `.veryl`.
#[test]
fn a_veryl_package_comes_back_with_the_name_the_author_gave_it() {
    let target = project().join("target");
    let design = lowered(&[target.join("defs.sv"), target.join("show.sv"), target.join("top.sv")]);
    assert_eq!(design.packages.len(), 1, "{:?}", design.packages);
    let defs = &design.packages[0];
    assert_eq!(defs.name, "lights_Defs", "as Veryl wrote it");
    assert_eq!(defs.written.as_deref(), Some("Defs"), "as the author did");
    assert!(file_of(&design.files, defs.span).ends_with("defs.veryl"));

    let show = design.module_by_name("Show").expect("Show");
    assert_eq!(show.imports.len(), 1);
    assert_eq!(show.imports[0].package, "lights_Defs");
    assert_eq!(show.imports[0].item, None);
    assert!(file_of(&design.files, show.imports[0].span).ends_with("show.veryl"));
    assert_eq!(show.imports[0].span.line, 1, "`import Defs::*;` is the first line");

    // Elaborated, the package's constant sizes the port of a module that
    // never imported it.
    let (design, diags) = rtlscope_elab::elaborate(&design, Some("Top"));
    let design = design.expect("elaborated");
    assert!(!diags.has_errors(), "{diags:?}");
    let top = design.module(design.top);
    let led = top.nets.iter().find(|n| n.name == "o_led").expect("o_led");
    assert_eq!(led.width, 4, "`logic<Defs::LEDS>`");
}
