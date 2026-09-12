//! Playing a drawn pattern for real, through cocotb on whichever simulator
//! is here — both, on a machine with both, because the point of having two
//! engines is that either one answers the same drawing the same way.
//!
//! The claim being tested is the one sentence everything else rests on:
//! **column `N` is the span between rising edge `N` and rising edge `N+1`**.
//! `pipeline3` is three registers deep, which makes that exact rather than
//! approximate — a beat driven into column 2 must come out in column 5, and an
//! off-by-one anywhere between the drawing and the simulator moves it.
//!
//! Running it also exercises everything the generated toolchain files do,
//! including building the VPI archive cocotb does not ship on Windows. The
//! first run on a machine fetches cocotb's sources once; after that it is
//! cached beside the harness.
//!
//! A missing simulator or venv skips rather than fails — a machine without
//! them has told us nothing about the code — but every skip says why.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rtlscope_sv::ParseOptions;
use rtlscope_tb::pattern::{Cell, Lane, Pattern, Verdict};
use rtlscope_tb::run::Engine;
use rtlscope_tb::{Flavor, Stimulus, TbOptions};

/// A beat through `pipeline3`, drawn.
///
/// In column 2 the input is valid and carries `0x0010`. Three registers later
/// — column 5 — `out_valid` must be high and `out_data` must be
/// `(0x10 + 1) ^ 0xffff`, which the fixture's own arithmetic decides.
fn one_beat() -> Pattern {
    let mut valid = Lane::driven("in_valid", 1);
    valid.set(2, Cell::Value(1));
    valid.set(3, Cell::Value(0));

    let mut data = Lane::driven("in_data", 16);
    data.set(2, Cell::Value(0x0010));
    // Deliberately unknown once the beat has been taken: the design must not be
    // looking at the data when `valid` is low, and driving `x` is how a
    // testbench says so.
    data.set(3, Cell::DontCare);

    let mut out_valid = Lane::expected("out_valid", 1);
    out_valid.set(5, Cell::Value(1));
    out_valid.set(6, Cell::Value(0));

    let mut out_data = Lane::expected("out_data", 16);
    out_data.set(5, Cell::Value((0x0010u64 + 1) ^ 0xffff));
    out_data.set(6, Cell::DontCare);

    Pattern {
        module: "pipeline3".into(),
        clock: "clk".into(),
        period_ns: 10,
        cycles: 10,
        drive: vec![valid, data],
        expect: vec![out_valid, out_data],
    }
}

/// Writes a harness for a design, whichever flavour.
fn harness(
    flavor: Flavor,
    pattern: &Pattern,
) -> Result<rtlscope_tb::Generated, rtlscope_tb::TbError> {
    let source = rtlscope_fixtures::path("pipeline3.sv");
    let (uir, _) =
        rtlscope_sv::lower_files(std::slice::from_ref(&source), &ParseOptions::default());
    let design = rtlscope_elab::elaborate(&uir, Some("pipeline3")).0.expect("elaborates");
    let (module, _) = design.module_by_name("pipeline3").expect("the top is in the design");

    let options = TbOptions {
        cycles: pattern.cycles + 4,
        stimulus: Stimulus::Drawn(Box::new(pattern.clone())),
        sources: vec![source.display().to_string()],
        ..TbOptions::default()
    };
    rtlscope_tb::generate_with(&design, module, &options, flavor)
}

/// The venv the README tells the reader to make.
///
/// On Windows it has to come from MSYS2's ucrt64 Python: cocotb's VPI is linked
/// into a binary that toolchain's gcc built, so an interpreter from anywhere
/// else cannot host it. The generated harness refuses such a Python by name;
/// here that case is a skip, because it is the machine's setup rather than the
/// code under test.
fn venv_python() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    for candidate in ["bin/python.exe", "Scripts/python.exe", "bin/python3", "bin/python"] {
        let python = root.join(".venv-cocotb").join(candidate);
        if python.is_file() {
            return Some(python);
        }
    }
    None
}

/// One playback at a time. Two of these may not run together on Windows, for
/// two separate reasons, one per engine.
///
/// **Icarus.** Its FST writer makes a scratch file of its own before it opens
/// the one it was asked for, and two started together can be handed the same
/// name for it. The loser reports `Unable to open <the output> for output` —
/// naming the file it was given rather than the one it could not make, which
/// is why it reads as a permission problem with a path that is plainly
/// writable. Measured 2026-09-10: two of these in parallel failed 3 runs out
/// of 8, and 0 out of 8 one at a time.
///
/// **Verilator.** cocotb ships no VPI library for it on Windows, so the
/// harness builds one into cocotb's own `libs` directory — *one* archive,
/// shared by every run on the machine. When it has to be rebuilt, the builder
/// unlinks it while the other run's linker still has it open, and Windows
/// refuses: `The process cannot access the file because it is being used by
/// another process`. The Python side takes a lock against a second *builder*,
/// which is not the same thing as a *reader*.
///
/// Neither is a fault in what is under test, and neither is worth working
/// around in the product: a queue here costs a few seconds and keeps both
/// engines honest. `--test-threads=1` proving a failure green is the fast way
/// to tell one of these from a real one.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Writes the harness for a pattern and plays it, the way a window or the
/// command line does: through the runner, which writes the files, starts the
/// venv's Python on `run.py`, and reads back where the waveform went. The
/// generated `run.py` finds the toolchain on its own.
///
/// `what` names the scratch directory, because two Verilator builds sharing one
/// would delete each other's object files halfway through the link, and the
/// error that came back would blame the linker.
fn play(what: &str, pattern: &Pattern, engine: Engine) -> Option<(Verdict, tempish::Dir)> {
    let Some(python) = venv_python() else {
        eprintln!("skipping: no `.venv-cocotb` in this checkout, so cocotb is not installed");
        return None;
    };
    if !rtlscope_tb::run::available(engine) {
        eprintln!("skipping: {} is not on this machine", engine.name());
        return None;
    }
    // Held for the whole run, and taken back from a poisoned lock: one test
    // that panicked has already reported itself, and turning that into a
    // failure of every later playback would bury it.
    let _queued = ONE_AT_A_TIME.lock().unwrap_or_else(|it| it.into_inner());

    let generated = harness(Flavor::Cocotb, pattern).expect("a harness");
    let work = tempish::Dir::new(&format!("rtlscope-drawn-{}-{what}", engine.name()));
    let tools = rtlscope_tb::Tools { python: Some(python), ..rtlscope_tb::Tools::default() };
    let played = rtlscope_tb::run::play(work.path(), "pipeline3", &generated.files, &tools, engine);
    let said = match played {
        Ok(outcome) => outcome.log,
        Err(rtlscope_tb::run::SimError::Cocotb { output })
            if output.contains("cannot run cocotb on Verilator here") =>
        {
            eprintln!("skipping: `.venv-cocotb` was not made from the ucrt64 Python");
            return None;
        }
        Err(error) => panic!("cocotb could not play the pattern on {}:\n{error}", engine.name()),
    };
    let banner = match engine {
        Engine::Verilator => "Running on Verilator",
        Engine::Icarus => "Running on Icarus",
    };
    assert!(said.contains(banner), "cocotb did not reach {}:\n{said}", engine.name());

    let verdict = Verdict::read(&work.path().join("pipeline3_verdict.json"))
        .unwrap_or_else(|why| panic!("cocotb left no verdict: {why}\n{said}"));
    Some((verdict, work))
}

/// The whole point: a drawing is a testcase, and it holds.
fn a_drawn_beat_arrives_where_it_was_drawn(engine: Engine) {
    let Some((verdict, _work)) = play("beat", &one_beat(), engine) else { return };

    assert!(
        verdict.checked >= 4,
        "only {} column(s) were checked — the expectations are not being read",
        verdict.checked
    );
    assert!(verdict.passed(), "the drawn beat did not arrive: {:?}", verdict.failures);
}

#[test]
fn a_drawn_beat_arrives_where_it_was_drawn_on_verilator() {
    a_drawn_beat_arrives_where_it_was_drawn(Engine::Verilator);
}

#[test]
fn a_drawn_beat_arrives_where_it_was_drawn_on_icarus() {
    a_drawn_beat_arrives_where_it_was_drawn(Engine::Icarus);
}

/// A checker that cannot fail is not a checker. This moves the expectation one
/// column early — where the beat provably is not — and demands to be told.
fn an_expectation_that_is_wrong_is_reported_with_its_column(engine: Engine) {
    let mut pattern = one_beat();
    pattern.expect[0].changes.clear();
    pattern.expect[0].set(4, Cell::Value(1)); // one column too early
    pattern.expect[0].set(5, Cell::Value(0));
    pattern.expect[1].changes.clear(); // and say nothing about the data

    let Some((verdict, _work)) = play("wrong", &pattern, engine) else { return };

    let at_four: Vec<&rtlscope_tb::Mismatch> =
        verdict.failures.iter().filter(|miss| miss.cycle == 4).collect();
    assert_eq!(
        at_four.len(),
        1,
        "column 4 was drawn wrong and should be the one that fails: {:?}",
        verdict.failures
    );
    assert_eq!(at_four[0].port, "out_valid");
    assert_eq!(at_four[0].expected, "1");
    assert_eq!(at_four[0].got, "0", "the beat is one column later than drawn");
}

#[test]
fn an_expectation_that_is_wrong_is_reported_with_its_column_on_verilator() {
    an_expectation_that_is_wrong_is_reported_with_its_column(Engine::Verilator);
}

#[test]
fn an_expectation_that_is_wrong_is_reported_with_its_column_on_icarus() {
    an_expectation_that_is_wrong_is_reported_with_its_column(Engine::Icarus);
}

/// The drawing travels beside the test, not inside it — which is what lets a
/// redraw run without building the design again.
#[test]
fn the_pattern_is_written_as_data_the_test_reads() {
    let generated = harness(Flavor::Cocotb, &one_beat()).expect("a harness");

    let names: Vec<&str> = generated.files.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"pipeline3_stim.json"),
        "the values are not written out at all: {names:?}"
    );

    let test = &generated.files[0].1;
    assert!(test.contains("_stim.json"), "the test does not read the pattern back");
    assert!(!test.contains("beef"), "a drawn value was baked into the test");
}

/// The SystemVerilog harness cannot read a pattern, and says so rather than
/// generating one that quietly drives nothing.
#[test]
fn asking_the_systemverilog_harness_for_a_pattern_is_refused() {
    let refused = harness(Flavor::Sv, &one_beat())
        .expect_err("a drawn pattern is not something that harness can play");

    let said = refused.to_string();
    assert!(said.contains("cocotb"), "and it says what to use instead: {said}");
}

/// Writes the cocotb harness somewhere a person can look at it.
///
/// Not a check of its own — the checks are above — but the generated Python is
/// the part nobody reads until it fails, so there is a way to read it.
#[test]
#[ignore = "writes files for a person to look at"]
fn emit_the_cocotb_harness() {
    let generated = harness(Flavor::Cocotb, &one_beat()).expect("a harness");
    let out = std::env::temp_dir().join("rtlscope-cocotb-drawn");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("a directory");
    for (name, contents) in &generated.files {
        std::fs::write(out.join(name), contents).expect("writes");
    }
    eprintln!("wrote {}", out.display());
}

/// A scratch directory that cleans itself up.
mod tempish {
    use super::{Path, PathBuf};

    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(name);
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a scratch directory");
            Dir(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
