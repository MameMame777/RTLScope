//! Writing a cocotb testbench and the script that runs it.
//!
//! cocotb drives the design from Python over the simulator's VPI, which suits
//! this well: the harness is generated once from the IR and then *edited* by
//! whoever knows what stimulus the design needs, rather than regenerated. So
//! what comes out is a working skeleton — clocks running, reset released, the
//! design ticking over — with the places to write real stimulus marked.
//!
//! Three details are here because getting them wrong costs an afternoon, and
//! all three were established by running it rather than by reading about it:
//!
//! - **A timescale is not optional.** Without one Icarus runs at 1-second
//!   precision and cocotb refuses to make a nanosecond clock. It is passed to
//!   the runner rather than written into the RTL, so the design is never
//!   touched.
//! - **`WAVES=1` is what actually produces a dump.** Asking the runner for
//!   waves is not enough on its own; without the environment variable the
//!   simulator is told to suppress dumping and writes nothing.
//! - **The top module must not be named twice.** cocotb passes its own `-s`,
//!   and a second one makes Icarus die with an access violation rather than a
//!   message.

use std::fmt::Write as _;
use std::path::PathBuf;

use rtlscope_ir::{Design, ModuleId, PortDir};

use crate::clocks::{ClockPlan, plan};

/// What drives the inputs that are neither clock nor reset.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Stimulus {
    /// Held at zero and named, so the places to write real stimulus are marked
    /// rather than guessed at. Nothing flows, which is the point: this is a
    /// harness to be edited.
    #[default]
    TieOff,
    /// Driven at random every cycle once reset is done.
    ///
    /// This is what makes a dump out of nothing but source. It knows no
    /// protocol — a `ready` that says wait is ignored — so what it produces is
    /// a *waveform*, not a verdict. That is exactly what the diagram views
    /// need and exactly what a testbench must not be mistaken for.
    Random { seed: u32 },
    /// A waveform someone drew: what to drive, column by column, and what to
    /// expect back.
    ///
    /// Unlike the other two this carries a *verdict* — the drawing says what
    /// the outputs should have been, so the run either holds or does not. And
    /// unlike a harness with the values written into it, the pattern is data
    /// the harness reads, so redrawing does not mean compiling the design
    /// again. See [`crate::pattern`].
    Drawn(Box<crate::pattern::Pattern>),
}

#[derive(Debug, Clone)]
pub struct TbOptions {
    /// How many cycles of the busiest clock to run for.
    pub cycles: u64,
    /// Parameter overrides, `NAME=value`. The lever for making a simulation
    /// that would take an hour take a second.
    pub overrides: Vec<(String, i64)>,
    /// Clock periods in nanoseconds, by port name, overriding the default.
    pub periods: Vec<(String, u32)>,
    /// Absolute paths of the sources to compile.
    pub sources: Vec<String>,
    /// Restrict the dump to one scope inside the design, rather than all of it.
    pub dump_scope: Option<String>,
    /// What drives everything that is not a clock or a reset.
    pub stimulus: Stimulus,
}

impl Default for TbOptions {
    fn default() -> Self {
        Self {
            cycles: 2_000,
            overrides: Vec::new(),
            periods: Vec::new(),
            sources: Vec::new(),
            dump_scope: None,
            stimulus: Stimulus::default(),
        }
    }
}

/// Files to write, and what the generator could not work out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    pub files: Vec<(String, String)>,
    pub notes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TbError {
    #[error(
        "this machine spells paths in code page {code_page}, and the simulator reaches the \
         filesystem through it — so it cannot open:\n    {}\n  The characters outside that \
         code page arrive as `?`, and what comes back is a complaint about a path that looks \
         nothing like the one it was given. Measured with both Verilator and Icarus; a `-f` \
         file list does not help, because the loss is at the open and not in the argument.\n  \
         Move the design somewhere this code page can spell, or turn on \"Beta: Use Unicode \
         UTF-8 for worldwide language support\" in Windows' region settings, which makes the \
         code page UTF-8.",
        .paths.join("\n    ")
    )]
    PathOutsideCodePage { paths: Vec<String>, code_page: u32 },
    #[error("no module named `{0}` in this design")]
    NoModule(String),
    #[error("`{0}` is a black box — there is no source to simulate")]
    Blackbox(String),
    #[error("`{0}` is not a parameter of this module that can be overridden")]
    NoSuchParameter(String),
    #[error("could not read `{path}`: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("`{path}` has no <testcase> in it, so it is not a results file a run wrote")]
    NoTests { path: String },
    #[error(
        "a drawn pattern is played by cocotb, which reads it as data — the SystemVerilog \
         harness has no way to read one. Generate it with `--flavor cocotb`."
    )]
    DrawnIsCocotbOnly,
    #[error("`{0}` has no clock, so a drawn pattern has no columns to count")]
    NoClock(String),
}

pub fn generate(
    design: &Design,
    module_id: ModuleId,
    options: &TbOptions,
) -> Result<Generated, TbError> {
    let module = &design.modules[module_id];
    if module.is_blackbox {
        return Err(TbError::Blackbox(module.name.clone()));
    }

    for (name, _) in &options.overrides {
        if !module.params.iter().any(|p| &p.name == name && !p.is_local) {
            return Err(TbError::NoSuchParameter(name.clone()));
        }
    }

    let mut plan = plan(design, module_id);
    for (port, period) in &options.periods {
        match plan.clocks.iter_mut().find(|c| &c.port == port) {
            Some(clock) => clock.period_ns = *period,
            None => plan.notes.push(format!("`{port}` is not a clock of this module")),
        }
    }

    // `base_name` is the name in the source; `name` may carry a parameter
    // specialisation suffix that the simulator has never heard of.
    let top = module.base_name.clone();
    let mut files = vec![
        (format!("test_{top}.py"), test_module(design, module_id, &plan, options)),
        ("run.py".to_string(), runner(&top, &plan, options)),
        ("README.md".to_string(), readme(&top, &plan)),
    ];

    // What cocotb needs to reach Verilator on this platform. Written always,
    // not only for a drawn pattern: `run.py` reads them either way.
    files.extend(crate::toolchain::files());

    // The drawn values travel beside the test rather than inside it, which is
    // what lets a redrawn pattern run without building the design again.
    if let Stimulus::Drawn(pattern) = &options.stimulus {
        files.push((format!("{top}_stim.json"), pattern.to_json()));
        files.push((format!("{top}_stim.vcd"), pattern.to_vcd()));
        plan.notes.extend(pattern.too_wide());
        plan.notes.retain(|note| !note.contains("tied off"));
        plan.notes.push(format!(
            "{} input(s) and {} output(s) come from the drawn pattern in `{top}_stim.json`; \
             the verdict is written to `{top}_verdict.json`.",
            pattern.drivable().len(),
            pattern.checkable().len()
        ));
    }
    Ok(Generated { files, notes: plan.notes.clone() })
}

/// The Python cocotb reads: a clock per clock, a reset sequence, and a test
/// that does nothing but let the design run.
fn test_module(
    design: &Design,
    module_id: ModuleId,
    plan: &ClockPlan,
    options: &TbOptions,
) -> String {
    let module = &design.modules[module_id];
    let mut out = String::new();

    let _ = writeln!(
        out,
        "\"\"\"Generated by `rtlscope tb-init` for `{}`.\n\n\
         The clocks and the reset were read out of the design, not guessed from\n\
         port names: every `always_ff` in the IR names the net it is clocked by\n\
         and the one that resets it.{}\n\"\"\"",
        module.base_name,
        match options.stimulus {
            Stimulus::Drawn(_) => {
                " Everything else comes from the\npattern beside this file. It is data: redraw it and run again without \
                 building the\ndesign a second time."
            }
            _ => " Everything else is left at zero — that is\nwhere your stimulus goes.",
        }
    );
    out.push_str("\nimport cocotb\n");
    out.push_str("from cocotb.clock import Clock\n");
    if matches!(options.stimulus, Stimulus::Drawn(_)) {
        out.push_str("from cocotb.triggers import ClockCycles, FallingEdge, RisingEdge\n");
        out.push_str("from cocotb.types import LogicArray\n");
        out.push_str("from cocotb.utils import get_sim_time\n");
        out.push_str("import json\n");
        out.push_str("import pathlib\n\n");
        let _ = writeln!(
            out,
            "_HERE = pathlib.Path(__file__).parent\n\
             _PATTERN = json.loads((_HERE / \"{}_stim.json\").read_text())\n\n",
            module.base_name
        );
    } else {
        out.push_str("from cocotb.triggers import ClockCycles, RisingEdge\n\n\n");
    }

    // ---- reset ----
    let _ = writeln!(out, "async def reset(dut):");
    if plan.resets.is_empty() {
        let _ =
            writeln!(out, "    \"\"\"Nothing in this design resets; here to be filled in.\"\"\"");
        let _ = writeln!(out, "    return");
    } else {
        let _ = writeln!(out, "    \"\"\"Holds every reset, then releases them together.\"\"\"");
        for reset in &plan.resets {
            let asserted = u8::from(!reset.active_low);
            let _ = writeln!(
                out,
                "    dut.{}.value = {asserted}  # active {}{}",
                reset.port,
                if reset.active_low { "low" } else { "high" },
                if reset.asynchronous { ", asynchronous" } else { "" }
            );
        }
        if let Some(clock) = plan.clocks.first() {
            let cycles = plan.resets.iter().map(|r| r.cycles).max().unwrap_or(5);
            let _ = writeln!(out, "    await ClockCycles(dut.{}, {cycles})", clock.port);
        }
        for reset in &plan.resets {
            let released = u8::from(reset.active_low);
            let _ = writeln!(out, "    dut.{}.value = {released}", reset.port);
        }
        if let Some(clock) = plan.clocks.first() {
            let _ = writeln!(out, "    await RisingEdge(dut.{})", clock.port);
        }
    }
    out.push_str("\n\n");

    // ---- the test ----
    if let Stimulus::Drawn(pattern) = &options.stimulus {
        drawn_test(&mut out, plan, pattern, &module.base_name);
        return out;
    }
    let _ = writeln!(out, "@cocotb.test()");
    let _ = writeln!(out, "async def runs(dut):");
    let _ = writeln!(
        out,
        "    \"\"\"Runs the design so that there is a waveform to look at.\n\n    \
         Replace the wait at the end with something that drives the inputs and\n    \
         checks the outputs; the point of this one is only to produce a dump.\n    \"\"\""
    );
    for clock in &plan.clocks {
        let _ = writeln!(
            out,
            "    cocotb.start_soon(Clock(dut.{}, {}, unit=\"ns\").start())  # {} flop(s)",
            clock.port, clock.period_ns, clock.flops
        );
    }

    // Inputs that are neither clock nor reset start at a defined value, so the
    // design is not driven by `x` from the first edge.
    if !plan.tied_off.is_empty() {
        out.push_str("\n    # Tied off so nothing starts undriven. Your stimulus goes here.\n");
        for port in &plan.tied_off {
            let _ = writeln!(out, "    dut.{port}.value = 0");
        }
    }

    if !plan.resets.is_empty() || !plan.clocks.is_empty() {
        let _ = writeln!(out, "\n    await reset(dut)");
    }
    match plan.clocks.first() {
        Some(clock) => {
            let _ = writeln!(out, "    await ClockCycles(dut.{}, {})", clock.port, options.cycles);
        }
        None => {
            out.push_str("    # No clock was found, so there is nothing to wait on.\n");
            out.push_str("    from cocotb.triggers import Timer\n");
            let _ = writeln!(out, "    await Timer({}, unit=\"ns\")", options.cycles * 10);
        }
    }

    let outputs: Vec<&str> = module
        .ports
        .iter()
        .filter(|p| p.dir == PortDir::Output)
        .map(|p| p.name.as_str())
        .take(4)
        .collect();
    if !outputs.is_empty() {
        out.push('\n');
        for name in outputs {
            let _ = writeln!(out, "    dut._log.info(f\"{name} = {{dut.{name}.value}}\")");
        }
    }
    out
}

/// A test that plays a drawn pattern and checks what it said to expect.
///
/// This is where cocotb earns its place. The pattern is data the test reads, so
/// redrawing and running again compiles nothing — the design was built once. A
/// harness with the values written into it would mean a rebuild per edit, which
/// on Verilator is ten seconds, and on a drawing loop that is fatal.
///
/// The timing is [`crate::pattern`]'s one rule: column `N` is the span between
/// rising edge `N` and rising edge `N+1`. So the inputs are set at the start of
/// the span and the outputs read in its middle — at the falling edge, where
/// everything has settled and nothing is racing an edge.
fn drawn_test(out: &mut String, plan: &ClockPlan, pattern: &crate::Pattern, top: &str) {
    out.push_str(
        "def _held(lane, column):\n    \
         \"\"\"What a lane holds during a column: the last change at or before it.\n\n    \
         Before the first change a lane holds its `initial`: zero for a row that\n    \
         drives, `x` for a row that expects. An undrawn expectation is not an\n    \
         expectation of zero.\n    \"\"\"\n    \
         held = lane[\"initial\"]\n    \
         for at, value in lane[\"changes\"]:\n        \
         if at > column:\n            \
         break\n        \
         held = value\n    \
         return held\n\n\n",
    );
    out.push_str(
        "def _drive(dut, lane, column):\n    \
         held = _held(lane, column)\n    \
         port = getattr(dut, lane[\"port\"])\n    \
         if held == \"x\":\n        \
         port.value = LogicArray(\"X\" * lane[\"width\"])\n    \
         else:\n        \
         port.value = int(held, 16)\n\n\n",
    );
    out.push_str(
        "def _check(dut, lane, column, verdict):\n    \
         want = _held(lane, column)\n    \
         if want == \"x\":\n        \
         return  # not drawn, so not checked\n    \
         verdict[\"checked\"] += 1\n    \
         raw = getattr(dut, lane[\"port\"]).value\n    \
         try:\n        \
         got = int(raw)\n    \
         except ValueError:\n        \
         got = None  # it holds x or z, which is never what was drawn\n    \
         if got == int(want, 16):\n        \
         return\n    \
         verdict[\"failures\"].append(\n        \
         {\n            \
         \"cycle\": column,\n            \
         \"time_ns\": get_sim_time(unit=\"ns\"),\n            \
         \"port\": lane[\"port\"],\n            \
         \"expected\": want,\n            \
         \"got\": format(got, \"x\") if got is not None else str(raw),\n        \
         }\n    \
         )\n\n\n",
    );

    out.push_str("@cocotb.test()\n");
    out.push_str("async def drawn(dut):\n");
    out.push_str(
        "    \"\"\"Plays the pattern that was drawn, and checks what it expects.\n\n    \
         Column N is the span between rising edge N and rising edge N+1: the\n    \
         inputs are set at its start and the outputs read in its middle, where\n    \
         they have settled.\n    \"\"\"\n",
    );
    for clock in &plan.clocks {
        let _ = writeln!(
            out,
            "    cocotb.start_soon(Clock(dut.{}, {}, unit=\"ns\").start())",
            clock.port, clock.period_ns
        );
    }
    if !plan.resets.is_empty() || !plan.clocks.is_empty() {
        out.push_str("    await reset(dut)\n");
    }
    let Some(clock) = plan.clocks.first() else {
        out.push_str(
            "    assert False, \"this module has no clock, so a pattern has no columns\"\n",
        );
        return;
    };

    out.push_str("\n    verdict = {\"checked\": 0, \"failures\": []}\n");
    out.push_str("    for column in range(_PATTERN[\"cycles\"]):\n");
    if !pattern.drivable().is_empty() {
        out.push_str("        for lane in _PATTERN[\"drive\"]:\n");
        out.push_str("            _drive(dut, lane, column)\n");
    }
    let _ = writeln!(out, "        await FallingEdge(dut.{})", clock.port);
    if !pattern.checkable().is_empty() {
        out.push_str("        for lane in _PATTERN[\"expect\"]:\n");
        out.push_str("            _check(dut, lane, column, verdict)\n");
    }
    let _ = writeln!(out, "        await RisingEdge(dut.{})", clock.port);

    let _ = writeln!(out, "\n    (_HERE / \"{top}_verdict.json\").write_text(json.dumps(verdict))");
    out.push_str(
        "    assert not verdict[\"failures\"], (\n        \
         f\"{len(verdict['failures'])} drawn expectation(s) failed, \"\n        \
         f\"first at column {verdict['failures'][0]['cycle']}\"\n    )\n",
    );
}

/// The script that builds and runs it.
///
/// Verilator by default: it is the faster simulator and the one this project
/// already knows how to drive. Icarus stays reachable by one word because
/// cocotb ships its VPI ready-made, which makes it the thing to fall back to
/// if a cocotb or Verilator upgrade ever breaks the VPI build.
///
/// Everything Windows needs to make that combination work lives in the files
/// [`crate::toolchain`] writes beside this one, and is applied here in order:
/// the interpreter is checked first, because using the wrong one produces no
/// complaint until the link fails and then blames a missing library.
fn runner(top: &str, plan: &ClockPlan, options: &TbOptions) -> String {
    let mut out = String::new();
    out.push_str(
        "\"\"\"Builds and runs the generated testbench.\n\n\
         Everything platform-specific is in `rtlscope_site.py` beside this file;\n\
         on Linux and macOS it does nothing at all.\n\"\"\"\n\n",
    );
    out.push_str("import os\nimport sys\nfrom pathlib import Path\n\n");
    out.push_str("here = Path(__file__).resolve().parent\n");
    out.push_str("sys.path.insert(0, str(here))\n\n");
    out.push_str("import rtlscope_site as site\nimport rtlscope_vpi as vpi\n\n");
    out.push_str("engine = os.environ.get(\"RTLSCOPE_ENGINE\", \"verilator\")\n\n");

    out.push_str(
        "# The order matters: the toolchain has to be findable before the\n\
         # interpreter can be checked against it, and the VPI archive has to\n\
         # exist before the runner tries to link against it. Only Verilator asks\n\
         # for either: cocotb ships its Icarus VPI on every platform, and Icarus\n\
         # takes whichever Python cocotb was installed into.\n",
    );
    out.push_str("root = site.prepend_path(engine)\n");
    out.push_str("if engine == \"verilator\":\n");
    out.push_str("    site.check_interpreter(root)\n");
    out.push_str("    vpi.ensure()\n");
    out.push_str(
        "if site.WINDOWS:\n    \
             os.environ[\"COCOTB_DLL_DIRS\"] = site.sim_dll_dirs(root)\n\n",
    );

    out.push_str(
        "# Asking the runner for waves is not enough on its own: without this the\n\
         # simulator is told to suppress dumping and writes nothing at all.\n",
    );
    out.push_str("os.environ[\"WAVES\"] = \"1\"\n\n");

    out.push_str("sources = [\n");
    for source in &options.sources {
        let _ = writeln!(out, "    r\"{source}\",");
    }
    out.push_str("]\n\n");

    if let Some(scope) = &options.dump_scope {
        let _ = writeln!(
            out,
            "# A dump of just `{scope}` rather than the whole design. Written as its\n\
             # own root module so that cocotb's own dump does not also run."
        );
        out.push_str("sources.append(str(here / \"rtlscope_dump.sv\"))\n\n");
    }

    out.push_str("from cocotb_tools.runner import get_runner  # noqa: E402\n\n");
    out.push_str("runner = get_runner(engine)\n");
    out.push_str(
        "build_args = site.common_build_args() if engine == \"verilator\" else [\"-g2012\"]\n",
    );
    out.push_str("runner.build(\n");
    out.push_str("    sources=sources,\n");
    let _ = writeln!(out, "    hdl_toplevel=\"{top}\",");
    out.push_str("    build_args=build_args,\n");
    if !options.overrides.is_empty() {
        out.push_str("    parameters={\n");
        for (name, value) in &options.overrides {
            let _ = writeln!(out, "        \"{name}\": {value},");
        }
        out.push_str("    },\n");
    }
    // Without a timescale Icarus runs at 1-second precision and cocotb cannot
    // make a nanosecond clock. Passing it here leaves the RTL untouched.
    out.push_str("    timescale=(\"1ns\", \"1ps\"),\n");
    out.push_str("    build_dir=here / \"sim\",\n");
    out.push_str("    waves=True,\n");
    out.push_str("    always=True,\n");
    out.push_str(")\n\n");

    out.push_str("runner.test(\n");
    let _ = writeln!(out, "    hdl_toplevel=\"{top}\",");
    let _ = writeln!(out, "    test_module=\"test_{top}\",");
    out.push_str("    test_dir=here,\n");
    out.push_str("    build_dir=here / \"sim\",\n");
    out.push_str("    waves=True,\n");
    out.push_str(")\n\n");

    // Where the waveform went is the simulator's decision, not something to
    // guess from here: cocotb's Verilator runner writes `dump.vcd` beside the
    // test, Icarus an FST under the build directory. Printed in a form the
    // caller can read back, because a caller that guessed would be wrong for
    // one of them.
    match &options.dump_scope {
        Some(_) => {
            let _ = writeln!(out, "dump = here / \"{top}_scope.fst\"");
        }
        None => {
            let _ = writeln!(
                out,
                "dump = here / (\"dump.vcd\" if engine == \"verilator\" else \"sim/{top}.fst\")"
            );
        }
    }
    out.push_str("print(f\"RTLSCOPE_DUMP={dump}\")\n");
    if matches!(options.stimulus, Stimulus::Drawn(_)) {
        let _ = writeln!(out, "print(f\"verdict : {{here / '{top}_verdict.json'}}\")");
    }

    let _ = plan;
    out
}

/// Every path the generated `run.py` can name as its waveform, relative to the
/// directory it runs in.
///
/// Beside the code that writes those names so the two cannot drift apart. A
/// caller that cleared the wrong one would leave an earlier run's waveform
/// where this one's belongs, and hand it to the reader as this one's.
pub fn dump_names(top: &str) -> Vec<PathBuf> {
    vec![
        PathBuf::from("dump.vcd"),
        PathBuf::from("sim").join(format!("{top}.fst")),
        PathBuf::from(format!("{top}_scope.fst")),
    ]
}

fn readme(top: &str, plan: &ClockPlan) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# `{top}` testbench\n");
    out.push_str("Generated by `rtlscope tb-init`. Run it from **PowerShell**:\n\n");
    out.push_str("```powershell\n");
    out.push_str("<venv>\\Scripts\\python.exe run.py\n");
    out.push_str("```\n\n");
    out.push_str(
        "cocotb must be installed in that environment, and Icarus must be on the\n\
         path. Started from Git Bash, Icarus exits 127 with an empty stderr, which\n\
         reads exactly like success — hence PowerShell.\n\n",
    );

    out.push_str("## What was read out of the design\n\n");
    if plan.clocks.is_empty() {
        out.push_str("- no clock: nothing in this module clocks anything\n");
    }
    for clock in &plan.clocks {
        let _ = writeln!(
            out,
            "- clock `{}` at {} ns, driving {} flop(s)",
            clock.port, clock.period_ns, clock.flops
        );
    }
    for reset in &plan.resets {
        let _ = writeln!(
            out,
            "- reset `{}`, active {}{}",
            reset.port,
            if reset.active_low { "low" } else { "high" },
            if reset.asynchronous { ", asynchronous" } else { ", synchronous" }
        );
    }
    if !plan.tied_off.is_empty() {
        let _ = writeln!(out, "- tied off, awaiting real stimulus: {}", plan.tied_off.join(", "));
    }

    out.push_str("\n## Then\n\n");
    out.push_str("```powershell\n");
    let _ = writeln!(out, "rtlscope wave-info sim\\{top}.fst <sources> --top {top}");
    let _ = writeln!(out, "rtlscope decode sim\\{top}.fst --protocol <name> --auto <instance>");
    out.push_str("```\n");
    out
}

/// The dedicated dump module, when only part of the design is wanted.
pub fn dump_module(top: &str, scope: &str, file: &str) -> String {
    format!(
        "// Generated by `rtlscope tb-init`. Its own root module, so that cocotb's\n\
         // whole-design dump does not also run and race this one for the file.\n\
         module rtlscope_dump ();\n    \
         initial begin\n        \
         $dumpfile(\"{file}\");\n        \
         $dumpvars(0, {top}.{scope});\n    \
         end\nendmodule\n"
    )
}
