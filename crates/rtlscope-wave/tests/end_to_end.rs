//! Source in, waveform out, and the waveform lined up with the source again.
//!
//! This is the only test that runs a simulator, and it is the one that would
//! catch a break anywhere along the chain: the testbench generator, cocotb, the
//! simulator, the FST reader, and the matcher all have to agree for it to pass.
//!
//! It skips rather than fails when the tools are not there. cocotb lives in a
//! virtualenv that a fresh checkout will not have, and a test that failed for
//! that reason would say nothing about the code — and would train whoever saw
//! it to ignore a real failure later.

use std::path::{Path, PathBuf};
use std::process::Command;

use rtlscope_analyse::flat;
use rtlscope_ir::Design;
use rtlscope_sv::ParseOptions;
use rtlscope_wave::{Dump, match_signals};

fn workspace_root() -> PathBuf {
    // `crates/rtlscope-wave` -> the workspace.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("workspace root")
}

/// The Python that has cocotb in it, or `None` if this checkout has none.
///
/// See the README: the default `python` here is MSYS2's, which cannot install
/// cocotb, so it lives in a virtualenv built from a native Python.
fn cocotb_python() -> Option<PathBuf> {
    let candidate = workspace_root().join(".venv-cocotb/Scripts/python.exe");
    if !candidate.exists() {
        return None;
    }
    let ok = Command::new(&candidate)
        .args(["-c", "import cocotb_tools.runner"])
        .status()
        .is_ok_and(|status| status.success());
    ok.then_some(candidate)
}

fn design(fixture: &str, top: Option<&str>) -> Design {
    let path = rtlscope_fixtures::path(fixture);
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, top);
    design.expect("elaboration produced a design")
}

#[test]
fn a_generated_testbench_produces_a_dump_that_matches_the_design() {
    let Some(python) = cocotb_python() else {
        eprintln!("skipping: no .venv-cocotb with cocotb in it");
        return;
    };

    let fixture = rtlscope_fixtures::path("counter.sv");
    let design = design("counter.sv", None);
    let module = design.top;

    // 1. Generate the harness, exactly as `rtlscope tb-init` would.
    let options = rtlscope_tb::TbOptions {
        cycles: 40,
        sources: vec![fixture.display().to_string()],
        ..rtlscope_tb::TbOptions::default()
    };
    let generated = rtlscope_tb::generate(&design, module, &options).expect("a testbench");

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e_counter");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a place to write it");
    for (name, contents) in &generated.files {
        std::fs::write(dir.join(name), contents).expect("writing the testbench");
    }

    // 2. Run it. Icarus must be on the path; without it there is nothing to run.
    let output = Command::new(&python)
        .arg("run.py")
        .current_dir(&dir)
        .output()
        .expect("starting the runner");
    let log = String::from_utf8_lossy(&output.stderr).to_string()
        + &String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || log.contains("Icarus Verilog not found") {
        eprintln!("skipping: the simulation did not run\n{log}");
        return;
    }
    assert!(log.contains("PASS=1"), "the generated testbench did not pass:\n{log}");

    // 3. Read what it wrote.
    let fst = dir.join("sim/counter.fst");
    assert!(fst.exists(), "no waveform at {}:\n{log}", fst.display());
    let mut dump = Dump::open(&fst).expect("the FST opens");

    // 4. Line it up with the design it came from.
    let flat = flat::flatten(&design);
    let report = match_signals(&dump, &design, &flat, None);
    assert!(
        report.matched.len() >= 4,
        "only {} of {} signals matched under `{}`:\n{:#?}",
        report.matched.len(),
        report.prefix_score.1,
        report.prefix,
        report.unmatched_ir
    );

    // 5. And read a value back through the whole chain.
    let clk = report.by_ir_name("clk").expect("the clock matched");
    dump.load(&[clk.var]).expect("loading the clock");
    let edges = dump.changes(clk.var).expect("clock changes").count();
    assert!(edges > 40, "a clock that ran for 40 cycles should have changed more than {edges}×");

    let count = report.by_ir_name("count").expect("`count` matched");
    assert_eq!(count.width, 8, "the width came through the dump intact");
}
