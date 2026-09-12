//! Running a generated harness for real, on whichever simulator is here.
//!
//! These drive the actual tools, because the whole point of the runner is what
//! happens between this process and another one — the arguments, the working
//! directory, the environment, whether a dump appears where it was promised.
//! None of that can be tested by inspecting a string.
//!
//! A missing simulator skips rather than fails, on the same principle as the
//! fixture legality test: a machine without Verilator has not told us anything
//! about the code. But the skip *says so*, because a test suite that quietly
//! runs nothing is the failure mode this arrangement exists to avoid.

use std::path::PathBuf;

use rtlscope_sv::ParseOptions;
use rtlscope_tb::run::{Engine, SimError};
use rtlscope_tb::{Flavor, Stimulus, TbOptions};

/// Generates a randomly-driven harness for a fixture and simulates it.
fn dump_of(engine: Engine, fixture: &str, top: &str) -> Option<(PathBuf, tempish::Dir)> {
    if !rtlscope_tb::run::available(engine) {
        eprintln!("skipping {}: not on this machine", engine.name());
        return None;
    }
    let source = rtlscope_fixtures::path(fixture);
    let (uir, _) =
        rtlscope_sv::lower_files(std::slice::from_ref(&source), &ParseOptions::default());
    let design = rtlscope_elab::elaborate(&uir, Some(top)).0.expect("elaborates");
    let (module, _) = design.module_by_name(top).expect("the top is in the design");

    let options = TbOptions {
        cycles: 200,
        stimulus: Stimulus::Random { seed: 1 },
        sources: vec![source.display().to_string()],
        ..TbOptions::default()
    };
    let generated =
        rtlscope_tb::generate_with(&design, module, &options, Flavor::Sv).expect("a harness");

    let work = tempish::Dir::new(&format!("rtlscope-sim-test-{}-{top}", engine.name()));
    match rtlscope_tb::simulate(
        engine,
        work.path(),
        top,
        &generated.files,
        &[source],
        &rtlscope_tb::Tools::default(),
    ) {
        Ok(outcome) => Some((outcome.dump, work)),
        Err(SimError::ToolMissing { tool, .. }) => {
            eprintln!("skipping {}: `{tool}` is not on the PATH", engine.name());
            None
        }
        Err(error) => panic!("{} could not simulate {fixture}:\n{error}", engine.name()),
    }
}

/// The claim the whole feature rests on: sources in, a dump out, with the
/// pipeline actually carrying something.
fn a_driven_pipeline_comes_out(engine: Engine) {
    let Some((dump, _work)) = dump_of(engine, "pipeline3.sv", "pipeline3") else { return };

    let mut opened = rtlscope_wave::Dump::open(&dump).expect("the dump opens");
    let source = rtlscope_fixtures::path("pipeline3.sv");
    let (uir, _) = rtlscope_sv::lower_files(&[source], &ParseOptions::default());
    let design = rtlscope_elab::elaborate(&uir, Some("pipeline3")).0.expect("elaborates");
    let flat = rtlscope_analyse::flat::flatten(&design);
    let matched = rtlscope_wave::match_signals(&opened, &design, &flat, None);

    assert!(
        matched.matched.len() >= 10,
        "{} matched only {} signal(s) — the harness and the design have drifted apart",
        engine.name(),
        matched.matched.len()
    );

    // Random driving is the point: tied off, every one of these would be flat,
    // and the stage views would have nothing to show.
    let domain = rtlscope_analyse::pipeline::analyse(&design)
        .domains
        .into_iter()
        .next()
        .expect("pipeline3 has a clock domain");
    let cycles = rtlscope_wave::stages::cycles(&mut opened, &matched, &domain.clock)
        .expect("the clock is in the dump");
    assert!(cycles.len() > 100, "only {} cycle(s) ran", cycles.len());

    let layout = rtlscope_wave::Layout::window(0, cycles.len());
    let view = rtlscope_wave::stages::occupancy(&mut opened, &matched, &domain, &cycles, &layout);
    let busy: usize = view.rows.iter().map(|row| row.occupied).sum();
    assert!(
        busy > 20,
        "{} produced a dump with an idle pipeline ({busy} busy cell(s)) — random stimulus \
         is not reaching the design",
        engine.name()
    );

    // And the beats can be followed through it, which is what the flow view
    // draws.
    let flow = rtlscope_wave::flow::tokens(&view);
    assert!(!flow.is_empty(), "nothing could be followed through the pipe");
}

#[test]
fn verilator_turns_sources_into_a_running_pipeline() {
    a_driven_pipeline_comes_out(Engine::Verilator);
}

#[test]
fn icarus_turns_sources_into_a_running_pipeline() {
    a_driven_pipeline_comes_out(Engine::Icarus);
}

/// The failure a user is most likely to meet, and the one where a bad message
/// costs the most.
#[test]
fn a_simulator_that_is_not_there_says_how_to_get_it() {
    let work = tempish::Dir::new("rtlscope-sim-test-missing");
    let error = rtlscope_tb::simulate(
        Engine::Verilator,
        work.path(),
        "nothing",
        &[("tb_nothing.sv".to_string(), "module tb_nothing(); endmodule\n".to_string())],
        &[],
        &rtlscope_tb::Tools::default(),
    );
    // On a machine that *has* verilator this fails for another reason; either
    // way it must not succeed, and must not be silent about why.
    let said = error.expect_err("there is no design here to simulate").to_string();
    assert!(!said.is_empty());
    if said.contains("not on the PATH") {
        assert!(said.contains("pacman -S"), "{said}");
    }
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
            // Left behind on failure would be useful, but a test suite that
            // fills the temp directory is worse.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
