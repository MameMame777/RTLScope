//! Running a testbench somebody wrote, for real, on whichever simulator is here.
//!
//! The claim is the one the feature rests on: **the reader's own testbench runs
//! untouched, and its waveform comes back**. Nothing is generated over the top
//! of it, its checks are the ones that run, and the recording is of their
//! stimulus rather than of one this program invented.
//!
//! A missing simulator skips rather than fails — a machine without one has told
//! us nothing about the code — but every skip says why.

use std::path::PathBuf;
use std::sync::Mutex;

use rtlscope_sv::ParseOptions;
use rtlscope_tb::run::Engine;

/// One simulation at a time, for the two reasons `drawn.rs` gives at length:
/// two Icarus FST writers started together can be handed one scratch name, and
/// two Verilator builds share one VPI archive. Both were measured there; this
/// file learned it the same way, on the second full run after it was written.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// The fixture testbench, read the way a caller reads one.
fn bench(files: &[PathBuf], top: Option<&str>) -> rtlscope_tb::Bench {
    let (uir, _) = rtlscope_sv::lower_files(files, &ParseOptions::default());
    rtlscope_tb::bench::read(files, &uir, top).expect("a testbench")
}

/// Runs the fixture testbench against the fixture design.
fn run(engine: Engine, what: &str) -> Option<(rtlscope_tb::SimOutcome, tempish::Dir)> {
    if !rtlscope_tb::run::available(engine) {
        eprintln!("skipping {}: not on this machine", engine.name());
        return None;
    }
    let _queued = ONE_AT_A_TIME.lock().unwrap_or_else(|it| it.into_inner());
    let tb = vec![rtlscope_fixtures::path("counter_tb.sv")];
    let design = vec![rtlscope_fixtures::path("counter.sv")];
    let bench = bench(&tb, None);

    let work = tempish::Dir::new(&format!("rtlscope-bench-{}-{what}", engine.name()));
    match rtlscope_tb::bench::simulate(
        engine,
        work.path(),
        &bench,
        &design,
        &rtlscope_tb::Tools::default(),
    ) {
        Ok(outcome) => Some((outcome, work)),
        Err(rtlscope_tb::SimError::ToolMissing { tool, .. }) => {
            eprintln!("skipping {}: `{tool}` is not on the PATH", engine.name());
            None
        }
        Err(error) => panic!("{} could not run the testbench:\n{error}", engine.name()),
    }
}

/// The whole feature: their testbench, their checks, their waveform.
fn a_written_testbench_runs_and_records(engine: Engine) {
    let Some((outcome, _work)) = run(engine, "runs") else { return };

    // Its own `$display`, which is how a hand-written testbench says what it
    // found. If this is missing, something else ran.
    assert!(
        outcome.log.contains("counter_tb: PASS"),
        "the testbench's own checks did not run:\n{}",
        outcome.log
    );
    assert!(!outcome.log.contains("FAIL"), "{}", outcome.log);

    // And the recording is of that run.
    let dump = rtlscope_wave::Dump::open(&outcome.dump).expect("the dump opens");
    let names: Vec<String> = dump.vars().map(|(path, _)| path.to_string()).collect();
    assert!(
        names.iter().any(|name| name.contains("dut") && name.ends_with("count")),
        "the design under test is not in the recording: {names:?}"
    );
    assert!(dump.max_time() > 0, "nothing happened in it");
}

#[test]
fn a_written_testbench_runs_and_records_on_verilator() {
    a_written_testbench_runs_and_records(Engine::Verilator);
}

#[test]
fn a_written_testbench_runs_and_records_on_icarus() {
    a_written_testbench_runs_and_records(Engine::Icarus);
}

/// The dump the testbench asked for is the one that comes back — not a file
/// left behind by whatever ran in that directory before.
#[test]
fn the_waveform_is_the_one_this_run_wrote() {
    let Some((outcome, work)) = run(Engine::Icarus, "named") else { return };
    assert!(
        outcome.dump.starts_with(work.path()),
        "the dump came from somewhere else: {}",
        outcome.dump.display()
    );
    assert!(outcome.dump.is_file());
}

/// A testbench that records nothing still produces a recording, because one is
/// written beside it — and the design under test is in it, which is the point.
#[test]
fn a_testbench_that_records_nothing_is_given_a_recorder() {
    if !rtlscope_tb::run::available(Engine::Icarus) {
        eprintln!("skipping: icarus is not on this machine");
        return;
    }
    let _queued = ONE_AT_A_TIME.lock().unwrap_or_else(|it| it.into_inner());
    let work = tempish::Dir::new("rtlscope-bench-quiet");
    // The fixture with its dumping taken out, which is the ordinary state of a
    // testbench somebody wrote for a regression rather than for looking at.
    let text = std::fs::read_to_string(rtlscope_fixtures::path("counter_tb.sv")).expect("reads");
    let quiet = text
        .lines()
        .filter(|line| !line.contains("$dumpfile") && !line.contains("$dumpvars"))
        .collect::<Vec<_>>()
        .join("\n");
    let at = work.path().join("quiet_tb.sv");
    std::fs::write(&at, quiet).expect("writes");

    let files = vec![at];
    let bench = bench(&files, None);
    assert!(!bench.dumps, "it was supposed to have nothing to dump with");

    let design = vec![rtlscope_fixtures::path("counter.sv")];
    let outcome = rtlscope_tb::bench::simulate(
        Engine::Icarus,
        work.path(),
        &bench,
        &design,
        &rtlscope_tb::Tools::default(),
    )
    .expect("it runs");

    assert!(outcome.log.contains("counter_tb: PASS"), "{}", outcome.log);
    let dump = rtlscope_wave::Dump::open(&outcome.dump).expect("a recording all the same");
    let names: Vec<String> = dump.vars().map(|(path, _)| path.to_string()).collect();
    assert!(
        names.iter().any(|name| name.contains("count")),
        "the recorder recorded nothing useful: {names:?}"
    );
}

/// A scratch directory that cleans itself up.
mod tempish {
    use std::path::{Path, PathBuf};

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
