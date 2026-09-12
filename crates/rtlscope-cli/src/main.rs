//! RTLScope command line.
//!
//! Analysis results go to stdout as JSON; diagnostics go to stderr. Keeping the
//! two streams apart is what lets `rtlscope dump-ports top.sv | jq` work on a
//! design that also produced warnings.

use rtlscope_cli::{cmd, filelist};

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand, ValueEnum};
use rtlscope_ir::{Diagnostics, Severity};
use rtlscope_sv::ParseOptions;

#[derive(Parser)]
#[command(name = "rtlscope", version, about = "SystemVerilog design analysis")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report the modules, parameters, ports and instances the front end found.
    ///
    /// Runs before elaboration, so widths and parameter values are printed as
    /// the expressions they still are.
    DumpPorts {
        #[command(flatten)]
        sources: SourceArgs,
        /// Indent the JSON.
        #[arg(long)]
        pretty: bool,
    },

    /// Elaborate the design and report it: widths resolved, parameters applied,
    /// `generate` blocks unrolled, connections bound to nets.
    DumpIr {
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module. Inferred when exactly one module is instantiated by
        /// nothing else.
        #[arg(long)]
        top: Option<String>,
        /// Emit the IR itself rather than the readable summary. This is the
        /// JSON the MCP server will serve.
        #[arg(long)]
        raw: bool,
        /// Indent the JSON.
        #[arg(long)]
        pretty: bool,
    },

    /// Draw a module as a block diagram.
    /// List the state machines: their states, transitions and guards.
    ///
    /// A state machine is taken to be a register whose next value a `case` on
    /// itself decides — both the one-process and two-process styles. State
    /// names come from the enum or the localparams the source used.
    Fsm {
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module. Inferred when exactly one module is instantiated by
        /// nothing else.
        #[arg(long)]
        top: Option<String>,
        /// Only this module.
        #[arg(long)]
        module: Option<String>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    /// List the clock domains and the signals that cross between them.
    ///
    /// The design is flattened first, so a crossing between two instances is
    /// found even when both call their clock `clk`. Only the two-flop
    /// synchroniser is recognised; everything else is listed as unrecognised
    /// rather than as wrong.
    Cdc {
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module. Inferred when exactly one module is instantiated by
        /// nothing else.
        #[arg(long)]
        top: Option<String>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    /// Report inferred latches, dead signals and uninstantiated modules.
    ///
    /// A latch is combinational logic that does not assign a signal on every
    /// path through it, which makes synthesis build storage to hold the old
    /// value. Processes with a construct RTLScope could not model are left alone
    /// rather than guessed at.
    Lint {
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module. Inferred when exactly one module is instantiated by
        /// nothing else.
        #[arg(long)]
        top: Option<String>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    /// Report how many clocks deep the logic is, and what sits at each depth.
    ///
    /// A stage is a position in the register adjacency graph, not something the
    /// source declares, so this finds pipelines whether or not they were meant
    /// as such — and finds them across module boundaries.
    Pipeline {
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module. Inferred when exactly one module is instantiated by
        /// nothing else.
        #[arg(long)]
        top: Option<String>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    /// How many clocks lie between two signals.
    ///
    /// The count is the clock edges the value crosses on the way. The walk
    /// starts at the first signal's value, so its own register does not count;
    /// arriving at a register costs one, so the second signal's does. On a
    /// three-deep pipeline that makes the input three clocks from the output,
    /// and the last register nothing at all from the output it drives.
    ///
    /// Names may be given in full (`u_dsp.u_fir.acc`) or by their last part
    /// (`acc`) when only one signal wears it.
    /// What reaches a signal, or what it reaches.
    ///
    /// Two questions one walk answers: what decides this value, and what this
    /// value disturbs. Clocks and resets are left out of both — they decide
    /// when a value arrives rather than what it is, and following one would
    /// reach every register in the domain and from there the whole design.
    Cone {
        /// The signal to ask about.
        signal: String,
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module. Inferred when exactly one module is instantiated by
        /// nothing else.
        #[arg(long)]
        top: Option<String>,
        /// Walk downstream — what this signal decides — rather than upstream.
        #[arg(long)]
        loads: bool,
        /// How many hops to follow.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        #[arg(long)]
        json: bool,
        #[arg(long, requires = "json")]
        pretty: bool,
    },
    Depth {
        /// The signal the value leaves from.
        from: String,
        /// The signal it arrives at.
        to: String,
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module. Inferred when exactly one module is instantiated by
        /// nothing else.
        #[arg(long)]
        top: Option<String>,
        /// A recording, to measure what the value actually took.
        ///
        /// The structure says how many clock edges lie between the two; a
        /// recording says how long they took, which is a different number
        /// wherever anything on the road waits. Both are reported, and so is
        /// what their disagreement means.
        #[arg(long)]
        dump: Option<PathBuf>,
        /// The scope the design sits under in the dump. Inferred by counting.
        #[arg(long)]
        prefix: Option<String>,
        /// Which clock's edges to count cycles in. The road's own, by default.
        #[arg(long)]
        clock: Option<String>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    /// Write a cocotb testbench that runs this module and dumps a waveform.
    ///
    /// The clocks and the reset come out of the design rather than out of the
    /// port names: every `always_ff` in the IR says what clocks it and what
    /// resets it. Everything else is tied off and named, so the places to write
    /// real stimulus are marked rather than guessed at.
    TbInit {
        /// The module to drive. Use the name `dump-ir` reports.
        module: String,
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module, for elaboration. Inferred when exactly one module is
        /// instantiated by nothing else.
        #[arg(long)]
        top: Option<String>,
        /// Where to write. Defaults to `tb/<module>/`.
        #[arg(long, value_name = "DIR")]
        out_dir: Option<PathBuf>,
        /// How many cycles of the busiest clock to run for.
        #[arg(long, default_value_t = 2000)]
        cycles: u64,
        /// Override a parameter, as `NAME=VALUE`. The lever for making a long
        /// simulation short.
        #[arg(long = "override", value_name = "NAME=VALUE")]
        overrides: Vec<String>,
        /// Set a clock's period in nanoseconds, as `PORT=NS`.
        #[arg(long = "clock", value_name = "PORT=NS")]
        clocks: Vec<String>,
        /// Dump only this scope inside the design, rather than all of it.
        #[arg(long, value_name = "PATH")]
        dump_scope: Option<String>,
        /// Which harness to write. cocotb needs Python with cocotb in it; `sv`
        /// needs only a simulator.
        #[arg(long, value_enum, default_value_t = Flavor::Cocotb)]
        flavor: Flavor,
        /// A drawn pattern to play: what to drive column by column, and what
        /// to expect back. The harness reads it as data, so redrawing and
        /// running again builds nothing.
        #[arg(long, value_name = "FILE")]
        pattern: Option<PathBuf>,
        /// Overwrite files that are already there.
        #[arg(long)]
        force: bool,
    },

    /// List what a waveform dump holds, and how much of it the design accounts
    /// for.
    ///
    /// With sources, the dump's signals are matched against the design's — the
    /// scope the design sits under is worked out by counting — and the buses on
    /// a chosen module are proposed as `decode --map` arguments.
    WaveInfo {
        /// The VCD or FST to read.
        dump: PathBuf,
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module, for elaboration.
        #[arg(long)]
        top: Option<String>,
        /// The scope the design sits under in the dump. Inferred when left out.
        #[arg(long)]
        prefix: Option<String>,
        /// Propose bindings for the buses on this module.
        #[arg(long, value_name = "NAME")]
        module: Option<String>,
        /// Where that module sits, for naming its signals in the dump.
        #[arg(long, value_name = "PATH", default_value = "")]
        instance: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    /// Hold two dumps against each other.
    ///
    /// Says what each has that the other does not, then which shared signals
    /// differ and the first moment each does — in the *first* dump's ticks, so
    /// a moment can be taken straight to a viewer looking at it. Exits non-zero
    /// when they differ, so a regression can gate a build.
    WaveDiff {
        /// The recording being checked.
        dump: PathBuf,
        /// What to check it against — the known-good one.
        against: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    /// Read a protocol off a dump.
    ///
    /// Bind the decoder's channels with `--map role=signal`, or `--map
    /// role=!signal` for one recorded inverted, as an open-drain bus is. Run
    /// `wave-info --module` first to have them proposed.
    Decode {
        /// The VCD or FST to read. Not needed with `--list`.
        dump: Option<PathBuf>,
        /// Which protocol. `--list` names them all.
        #[arg(long, value_name = "NAME")]
        protocol: Option<String>,
        /// Bind one channel: `role=signal`, or `role=!signal` if it is recorded
        /// inverted.
        #[arg(long = "map", value_name = "ROLE=SIGNAL")]
        maps: Vec<String>,
        /// List the protocols and their channels, and stop.
        #[arg(long)]
        list: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    /// Lay the pipeline's stages against the dump's cycles.
    ///
    /// `pipeline` says which registers sit at which depth and a dump says what
    /// they held; this puts one against the other. Whether a stage was
    /// *occupied* is not recorded anywhere, so each row looks for the valid bit
    /// that carries the claim and says which one it used — or says instead that
    /// it fell back to showing movement.
    Stages {
        /// The VCD or FST to read.
        dump: PathBuf,
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module, for elaboration.
        #[arg(long)]
        top: Option<String>,
        /// The scope the design sits under in the dump. Inferred when left out.
        #[arg(long)]
        prefix: Option<String>,
        /// Which clock domain. Without it, the deepest one the dump recorded.
        #[arg(long, value_name = "NAME")]
        clock: Option<String>,
        /// The first cycle to show.
        #[arg(long = "from", value_name = "CYCLE", default_value_t = 0)]
        first: usize,
        /// How many cycles to show.
        #[arg(long, value_name = "N", default_value_t = 80)]
        count: usize,
        /// Name a stage's valid bit rather than letting it be guessed at, as
        /// `--valid 2=g_conv.v3`. The report says which rows had to fall back
        /// to showing movement, and those are the ones worth naming.
        #[arg(long = "valid", value_name = "STAGE=SIGNAL")]
        valids: Vec<String>,
        /// Name the register whose value fills a stage's cells, as
        /// `--value 2=g_conv.center3`. Whether a cell reads as a stall is
        /// decided by whether this repeated, so a wrong one makes a stage look
        /// stuck when it is not.
        #[arg(long = "value", value_name = "STAGE=SIGNAL")]
        values: Vec<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    /// Read a cocotb `results.xml`, and say where in a dump each test sits.
    ///
    /// The file records how long each test ran rather than when, so the moments
    /// reported are those durations added up in order — which the report says,
    /// rather than presenting an accumulated number as a recorded one.
    TbResults {
        /// The `results.xml` a run wrote.
        results: PathBuf,
        /// Convert each moment into ticks of this dump.
        #[arg(long, value_name = "DUMP")]
        dump: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    /// Simulate a module and open the waveform, from sources alone.
    ///
    /// Writes a harness — clocks and reset read out of the design, every other
    /// input driven at random — runs a simulator on it, and reports the dump.
    /// This knows no protocol: it produces a *waveform*, not a verdict. Use
    /// `tb-init` when you want a harness to write real checks into.
    Sim {
        /// The module to simulate. Use the name `dump-ir` reports.
        module: String,
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module, for elaboration.
        #[arg(long)]
        top: Option<String>,
        /// Which simulator. Verilator compiles the design and is fast; Icarus
        /// interprets it and needs no C++ compiler.
        #[arg(long, value_enum, default_value_t = SimEngine::Verilator)]
        engine: SimEngine,
        /// How many cycles of the busiest clock to run for.
        #[arg(long, default_value_t = 2000)]
        cycles: u64,
        /// What to seed the random stimulus with. The same seed gives the same
        /// waveform, so two runs can be compared.
        #[arg(long, default_value_t = 1)]
        seed: u32,
        /// Play a drawn pattern instead of random stimulus, and report whether
        /// what it expected held. This turns a run into a verdict.
        ///
        /// The pattern is played by cocotb, which reads it as data — so
        /// redrawing and running again builds nothing.
        #[arg(long, value_name = "FILE")]
        pattern: Option<PathBuf>,
        /// The Python to play a pattern with.
        ///
        /// Left out, a `.venv-cocotb` is looked for in and above the design,
        /// then the working directory, then beside this program —
        /// `RTLSCOPE_PYTHON` names one outright. On Windows, Verilator needs a
        /// venv made from MSYS2's ucrt64 Python, and the harness says so if it
        /// is not; Icarus takes any Python with cocotb in it.
        #[arg(long, value_name = "PATH")]
        python: Option<PathBuf>,
        /// Override a parameter, as `NAME=VALUE`. The lever for making a long
        /// simulation short.
        #[arg(long = "override", value_name = "NAME=VALUE")]
        overrides: Vec<String>,
        /// Set a clock's period in nanoseconds, as `PORT=NS`.
        #[arg(long = "clock", value_name = "PORT=NS")]
        clocks: Vec<String>,
        /// Run a testbench you wrote, instead of one generated from the
        /// design.
        ///
        /// The files are read for two facts and otherwise left alone: which
        /// module to start at — the one with no ports — and whether they
        /// already record a waveform. Yours is not edited, and its checks are
        /// the ones that run. Everything about the stimulus, the cycles and
        /// the drawn pattern is ignored, because your testbench decides all
        /// of that.
        #[arg(long = "testbench", value_name = "FILE")]
        testbench: Vec<PathBuf>,
        /// Which module in the testbench to start at, when more than one could
        /// be it.
        #[arg(long, value_name = "MODULE", requires = "testbench")]
        bench_top: Option<String>,
        /// Where to build and run. Defaults to `rtlscope-sim/<module>/`.
        #[arg(long, value_name = "DIR")]
        out_dir: Option<PathBuf>,
        /// Print everything the simulator said, even when it worked.
        #[arg(long)]
        verbose: bool,
    },

    /// Check the IR against a Yosys netlist of the same sources.
    ///
    /// Every other view derives from the IR, so every one of them is wrong the
    /// same way if the IR is — and tests written against the same front end
    /// cannot see it. Yosys reading the same files can. Produce the netlist
    /// with:
    ///
    ///   yosys -p "read_verilog -sv <FILES>; hierarchy -top <TOP>; proc; write_json out.json"
    ///
    /// `proc` is not optional: `write_json` refuses a module that still has
    /// processes in it.
    YosysCheck {
        /// The JSON `write_json` produced.
        netlist: PathBuf,
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module, for elaboration.
        #[arg(long)]
        top: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        pretty: bool,
    },

    Diagram {
        #[command(flatten)]
        sources: SourceArgs,
        /// Top module. Inferred when exactly one module is instantiated by
        /// nothing else.
        #[arg(long)]
        top: Option<String>,
        /// Draw this module instead of the top — the elaborated name, as
        /// `dump-ir` reports it.
        #[arg(long)]
        module: Option<String>,
        /// Where to write the SVG. Defaults to stdout.
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Draw clock and reset wires, which are hidden by default because they
        /// reach every flop and bury the structure.
        #[arg(long)]
        show_clocks: bool,
    },
}

/// Which simulator to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SimEngine {
    /// Compiles the design to C++. Fast, and needs a working C++ compiler.
    Verilator,
    /// Interprets the design. Slower, and needs nothing but itself.
    Icarus,
}

impl From<SimEngine> for rtlscope_tb::Engine {
    fn from(engine: SimEngine) -> Self {
        match engine {
            SimEngine::Verilator => rtlscope_tb::Engine::Verilator,
            SimEngine::Icarus => rtlscope_tb::Engine::Icarus,
        }
    }
}

/// Which testbench to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Flavor {
    /// Python stimulus over the simulator's VPI. Needs cocotb installed.
    Cocotb,
    /// A self-contained SystemVerilog file. Needs only a simulator.
    Sv,
}

#[derive(Args)]
struct SourceArgs {
    /// SystemVerilog source files — or Veryl: a `.veryl` file, a `Veryl.toml`,
    /// or a project directory holding one. A Veryl project is built with
    /// `veryl build` and read as the SystemVerilog it wrote; every location
    /// reported still points into the `.veryl`.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Read source paths from a file list, one per line (`//` comments allowed).
    #[arg(short = 'f', long = "file-list", value_name = "LIST")]
    file_lists: Vec<PathBuf>,

    /// Define a macro, as `-D NAME` or `-D NAME=VALUE`.
    #[arg(short = 'D', long = "define", value_name = "NAME[=VALUE]")]
    defines: Vec<String>,

    /// Add a directory to the `include` search path.
    #[arg(short = 'I', long = "include", value_name = "DIR")]
    include_paths: Vec<PathBuf>,

    /// How to print diagnostics.
    #[arg(long, value_enum, default_value_t = DiagFormat::Text)]
    diag_format: DiagFormat,

    /// Exit non-zero if anything was skipped, not just on hard errors.
    #[arg(long)]
    deny_warnings: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum DiagFormat {
    Text,
    Json,
}

impl SourceArgs {
    fn resolve_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
        let mut paths = self.files.clone();
        for list in &self.file_lists {
            paths.extend(filelist::read(list)?);
        }
        Ok(paths)
    }

    /// The paths named, with every Veryl project among them built and stood
    /// in for by the SystemVerilog it wrote.
    fn expand(&self) -> anyhow::Result<rtlscope_veryl::Expanded> {
        let paths = self.resolve_paths()?;
        if paths.is_empty() {
            anyhow::bail!("no source files given; pass files, a Veryl project, or -f <list>");
        }
        Ok(rtlscope_veryl::expand(&paths))
    }

    /// The sources, read: what every command starts from.
    fn lowered(&self) -> anyhow::Result<rtlscope_read::Lowered> {
        Ok(rtlscope_read::lower_expanded(self.expand()?, &self.parse_options()))
    }

    /// The same, resolved.
    ///
    /// `Keep`, because a `--top` on a command line was typed for this run:
    /// being told it is not there is the answer to what was asked, where a
    /// window's pinned top outlives the read it was chosen in.
    fn read(&self, top: Option<&str>) -> anyhow::Result<rtlscope_read::Read> {
        Ok(self.lowered()?.elaborate(top, rtlscope_read::StaleTop::Keep))
    }

    fn parse_options(&self) -> ParseOptions {
        ParseOptions {
            defines: rtlscope_read::parse_defines(&self.defines),
            include_paths: self.include_paths.clone(),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::DumpPorts { sources, pretty } => {
            let lowered = sources.lowered()?;
            let (design, diags) = (lowered.uir, lowered.diags);
            let report = cmd::dump_ports::build(&design);

            let json = if pretty {
                serde_json::to_string_pretty(&report)?
            } else {
                serde_json::to_string(&report)?
            };
            println!("{json}");

            report_diagnostics(&diags, &design.files, sources.diag_format)?;
            Ok(exit_code(&diags, sources.deny_warnings))
        }

        Command::DumpIr { sources, top, raw, pretty } => {
            let lowered = sources.lowered()?;
            let (uir, mut diags) = (lowered.uir, lowered.diags);
            let (design, elab_diags) = rtlscope_elab::elaborate(&uir, top.as_deref());
            diags.extend(elab_diags);

            // Elaboration can fail outright — no top, or a cycle — in which case
            // there is nothing to print but the reason.
            let Some(design) = design else {
                report_diagnostics(&diags, &uir.files, sources.diag_format)?;
                return Ok(ExitCode::FAILURE);
            };

            let json = match (raw, pretty) {
                (true, true) => serde_json::to_string_pretty(&design)?,
                (true, false) => serde_json::to_string(&design)?,
                (false, true) => serde_json::to_string_pretty(&cmd::dump_ir::build(&design))?,
                (false, false) => serde_json::to_string(&cmd::dump_ir::build(&design))?,
            };
            println!("{json}");

            report_diagnostics(&diags, &design.files, sources.diag_format)?;
            Ok(exit_code(&diags, sources.deny_warnings))
        }

        Command::Fsm { sources, top, module, json, pretty } => {
            let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
            let Some(design) = design else {
                report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
                return Ok(ExitCode::FAILURE);
            };

            let mut fsms = rtlscope_analyse::fsm::find(&design);
            if let Some(name) = &module {
                // By whichever name the reader has: the one shown, the one read,
                // or the one before specialisation.
                fsms.retain(|f| {
                    let module = &design.modules[f.module];
                    f.module_name == *name || module.name == *name || module.base_name == *name
                });
            }

            if json {
                let text = if pretty {
                    serde_json::to_string_pretty(&fsms)?
                } else {
                    serde_json::to_string(&fsms)?
                };
                println!("{text}");
            } else {
                print!("{}", cmd::analyse::fsms_text(&fsms, &design.files));
            }

            report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
            Ok(exit_code(&diags.1, sources.deny_warnings))
        }

        Command::Cdc { sources, top, json, pretty } => {
            let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
            let Some(design) = design else {
                report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
                return Ok(ExitCode::FAILURE);
            };

            let report = rtlscope_analyse::cdc::analyse(&design);
            if json {
                let text = if pretty {
                    serde_json::to_string_pretty(&report)?
                } else {
                    serde_json::to_string(&report)?
                };
                println!("{text}");
            } else {
                print!("{}", cmd::analyse::cdc_text(&report, &design.files));
            }

            report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
            Ok(exit_code(&diags.1, sources.deny_warnings))
        }

        Command::Lint { sources, top, json, pretty } => {
            let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
            let Some(design) = design else {
                report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
                return Ok(ExitCode::FAILURE);
            };

            let report = rtlscope_analyse::lint::analyse(&design);
            if json {
                let text = if pretty {
                    serde_json::to_string_pretty(&report)?
                } else {
                    serde_json::to_string(&report)?
                };
                println!("{text}");
            } else {
                print!("{}", cmd::analyse::lint_text(&report, &design.files));
            }

            report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
            Ok(exit_code(&diags.1, sources.deny_warnings))
        }

        Command::Pipeline { sources, top, json, pretty } => {
            let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
            let Some(design) = design else {
                report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
                return Ok(ExitCode::FAILURE);
            };

            let report = rtlscope_analyse::pipeline::analyse(&design);
            if json {
                let text = if pretty {
                    serde_json::to_string_pretty(&report)?
                } else {
                    serde_json::to_string(&report)?
                };
                println!("{text}");
            } else {
                print!("{}", cmd::analyse::pipeline_text(&report));
            }

            report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
            Ok(exit_code(&diags.1, sources.deny_warnings))
        }

        Command::Cone { signal, sources, top, loads, depth, json, pretty } => {
            let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
            let Some(design) = design else {
                report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
                return Ok(ExitCode::FAILURE);
            };

            let flat = rtlscope_analyse::flat::flatten(&design);
            let Some(root) = cmd::analyse::signal_named(&flat, &design, &signal) else {
                eprintln!("no signal called `{signal}` in this design");
                return Ok(ExitCode::FAILURE);
            };

            let graph = rtlscope_analyse::depth::signal_graph(&design, &flat);
            let cone = match loads {
                true => rtlscope_analyse::cone::fan_out(&graph, root, depth),
                false => rtlscope_analyse::cone::fan_in(&graph, root, depth),
            };

            if json {
                let named = cmd::analyse::cone_json(&cone, &flat);
                let text = match pretty {
                    true => serde_json::to_string_pretty(&named)?,
                    false => serde_json::to_string(&named)?,
                };
                println!("{text}");
            } else {
                print!("{}", cmd::analyse::cone_text(&cone, &flat));
            }

            report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Depth { from, to, sources, top, dump, prefix, clock, json, pretty } => {
            let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
            let Some(design) = design else {
                report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
                return Ok(ExitCode::FAILURE);
            };

            let report = rtlscope_analyse::depth::analyse(&design, &from, &to);

            // The recording, when there is one. Not attempted when the
            // structure already refused: measuring the distance between two
            // signals that have no road between them would put a number under a
            // refusal and invite it to be read as the answer.
            let measured = match (&dump, report.failed()) {
                (Some(path), false) => {
                    Some(measure_depth(path, &design, &report, prefix, clock, &from, &to)?)
                }
                _ => None,
            };
            let verdict = measured
                .as_ref()
                .map(|found| rtlscope_wave::cross_check(&report, found))
                .unwrap_or_default();

            if json {
                let both = serde_json::json!({
                    "static": &report,
                    "dynamic": &measured,
                    "cross_check": &verdict,
                });
                let text = if pretty {
                    serde_json::to_string_pretty(&both)?
                } else {
                    serde_json::to_string(&both)?
                };
                println!("{text}");
            } else {
                print!("{}", cmd::analyse::depth_text(&report, &design.files));
                if let Some(found) = &measured {
                    print!("{}", cmd::wave::latency_text(found, &verdict));
                }
            }

            report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
            // A question that could not be answered is a failure of the run,
            // not a report with a gap in it: a build step asking "is this
            // still three deep" has to be able to fail.
            if report.failed() {
                return Ok(ExitCode::FAILURE);
            }
            Ok(exit_code(&diags.1, sources.deny_warnings))
        }

        Command::TbInit {
            module,
            sources,
            top,
            out_dir,
            cycles,
            overrides,
            clocks,
            dump_scope,
            flavor,
            pattern,
            force,
        } => run_tb_init(
            module, sources, top, out_dir, cycles, overrides, clocks, dump_scope, flavor, pattern,
            force,
        ),

        Command::WaveInfo { dump, sources, top, prefix, module, instance, json, pretty } => {
            run_wave_info(dump, sources, top, prefix, module, instance, json, pretty)
        }

        Command::WaveDiff { dump, against, json, pretty } => {
            run_wave_diff(dump, against, json, pretty)
        }

        Command::Decode { dump, protocol, maps, list, json, pretty } => {
            run_decode(dump, protocol, maps, list, json, pretty)
        }

        Command::Stages {
            dump,
            sources,
            top,
            prefix,
            clock,
            first,
            count,
            valids,
            values,
            json,
            pretty,
        } => run_stages(
            dump, sources, top, prefix, clock, first, count, valids, values, json, pretty,
        ),

        Command::TbResults { results, dump, json, pretty } => {
            run_tb_results(results, dump, json, pretty)
        }

        Command::Sim {
            module,
            sources,
            top,
            engine,
            cycles,
            seed,
            pattern,
            python,
            overrides,
            clocks,
            testbench,
            bench_top,
            out_dir,
            verbose,
        } => run_sim(
            module,
            sources,
            top,
            engine,
            cycles,
            seed,
            pattern,
            python,
            overrides,
            clocks,
            Bench { files: testbench, top: bench_top },
            out_dir,
            verbose,
        ),

        Command::YosysCheck { netlist, sources, top, json, pretty } => {
            run_yosys_check(netlist, sources, top, json, pretty)
        }

        Command::Diagram { sources, top, module, out, show_clocks } => {
            run_diagram(sources, top, module, out, show_clocks)
        }
    }
}

/// Parse and elaborate, which every command past `dump-ports` starts with.
///
/// The file table comes back alongside the diagnostics because when
/// elaboration fails there is no design to take it from, and the reason still
/// has to be printed against real file names.
type Elaborated = (Option<rtlscope_ir::Design>, (rtlscope_ir::FileTable, rtlscope_ir::Diagnostics));

fn elaborate_sources(sources: &SourceArgs, top: Option<&str>) -> anyhow::Result<Elaborated> {
    let read = sources.read(top)?;
    Ok((read.design, (read.uir.files, read.diags)))
}

/// Writes a testbench for one module.
#[allow(clippy::too_many_arguments)]
fn run_tb_init(
    module: String,
    sources: SourceArgs,
    top: Option<String>,
    out_dir: Option<PathBuf>,
    cycles: u64,
    overrides: Vec<String>,
    clocks: Vec<String>,
    dump_scope: Option<String>,
    flavor: Flavor,
    pattern: Option<PathBuf>,
    force: bool,
) -> anyhow::Result<ExitCode> {
    let drawn = pattern.as_deref().map(read_pattern).transpose()?;
    // What the simulator is handed: the SystemVerilog, which for a Veryl
    // project is what Veryl wrote rather than what the reader named.
    let paths = sources.expand()?.sources;
    let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
    let Some(design) = design else {
        report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
        return Ok(ExitCode::FAILURE);
    };

    let Some((module_id, _)) = design.module_by_name(&module) else {
        let known: Vec<&str> = design.modules.iter().map(|m| m.name.as_str()).collect();
        anyhow::bail!("no module `{module}` in the design; have {known:?}");
    };

    let options = rtlscope_tb::TbOptions {
        cycles,
        overrides: parse_pairs(&overrides, "--override")?,
        periods: parse_pairs(&clocks, "--clock")?
            .into_iter()
            .map(|(name, value)| (name, value as u32))
            .collect(),
        // Absolute, because the generated script is run from its own directory.
        sources: paths
            .iter()
            .map(|path| {
                dunce::canonicalize(path).unwrap_or_else(|_| path.clone()).display().to_string()
            })
            .collect(),
        dump_scope: dump_scope.clone(),
        stimulus: match drawn {
            Some(pattern) => rtlscope_tb::Stimulus::Drawn(Box::new(pattern)),
            // The point of this command without a drawing: a harness with the
            // places to write stimulus marked rather than guessed at.
            None => rtlscope_tb::Stimulus::TieOff,
        },
    };

    let base = design.modules[module_id].base_name.clone();
    let flavor = match flavor {
        Flavor::Cocotb => rtlscope_tb::Flavor::Cocotb,
        Flavor::Sv => rtlscope_tb::Flavor::Sv,
    };
    let mut generated = rtlscope_tb::generate_with(&design, module_id, &options, flavor)?;
    // The SystemVerilog harness dumps from inside itself; cocotb's needs a
    // module of its own added to the sources.
    if let Some(scope) = &dump_scope
        && flavor == rtlscope_tb::Flavor::Cocotb
    {
        let file = format!("{base}_scope.fst");
        generated.files.push((
            "rtlscope_dump.sv".into(),
            rtlscope_tb::cocotb::dump_module(&base, scope, &file),
        ));
    }

    let dir = out_dir.unwrap_or_else(|| PathBuf::from("tb").join(&base));
    std::fs::create_dir_all(&dir)?;
    for (name, contents) in &generated.files {
        let path = dir.join(name);
        if path.exists() && !force {
            eprintln!("kept   {} (already there; --force to overwrite)", path.display());
            continue;
        }
        // A generated name may hold a directory of its own — the Perl shim goes
        // one level down, away from the working directory. See
        // `rtlscope_tb::toolchain::SHIM`.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
        eprintln!("wrote  {}", path.display());
    }
    for note in &generated.notes {
        eprintln!("note:  {note}");
    }
    eprintln!();
    match flavor {
        rtlscope_tb::Flavor::Cocotb => eprintln!(
            "run it from PowerShell:  <venv>/Scripts/python.exe {}",
            dir.join("run.py").display()
        ),
        rtlscope_tb::Flavor::Sv => {
            eprintln!("run it from PowerShell:  {}", dir.join("run.ps1").display())
        }
    }

    report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
    Ok(exit_code(&diags.1, sources.deny_warnings))
}

/// `STAGE=SIGNAL` pairs, where the stage is a number and the signal is a name.
fn parse_stage_pairs(given: &[String], flag: &str) -> anyhow::Result<Vec<(usize, String)>> {
    given
        .iter()
        .map(|entry| {
            let Some((stage, signal)) = entry.split_once('=') else {
                anyhow::bail!("{flag} wants STAGE=SIGNAL, got `{entry}`");
            };
            let stage: usize = stage
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("{flag} wants a stage number, got `{stage}`"))?;
            Ok((stage, signal.to_string()))
        })
        .collect()
}

/// `NAME=VALUE` pairs from the command line.
fn parse_pairs(given: &[String], flag: &str) -> anyhow::Result<Vec<(String, i64)>> {
    given
        .iter()
        .map(|pair| {
            let Some((name, value)) = pair.split_once('=') else {
                anyhow::bail!("{flag} wants NAME=VALUE, got `{pair}`");
            };
            let value: i64 = value
                .parse()
                .map_err(|_| anyhow::anyhow!("{flag} wants a number, got `{value}`"))?;
            Ok((name.to_string(), value))
        })
        .collect()
}

/// What a dump holds, and what of the design is in it.
#[allow(clippy::too_many_arguments)]
fn run_wave_info(
    dump: PathBuf,
    sources: SourceArgs,
    top: Option<String>,
    prefix: Option<String>,
    module: Option<String>,
    instance: String,
    json: bool,
    pretty: bool,
) -> anyhow::Result<ExitCode> {
    let dump_file = rtlscope_wave::Dump::open(&dump)?;
    let variables = dump_file.vars().count();

    // Without sources there is nothing to match against, so list what is there.
    let paths = sources.resolve_paths()?;
    if paths.is_empty() {
        let mut names: Vec<&str> = dump_file.vars().map(|(name, _)| name).collect();
        names.sort_unstable();
        if json {
            println!("{}", serde_json::to_string(&names)?);
        } else {
            println!("{variables} variable(s)");
            if let Some((factor, unit)) = dump_file.timescale() {
                println!("one tick is {factor} {unit}; the dump ends at {}", dump_file.max_time());
            }
            for name in &names {
                println!("  {name}");
            }
            println!("\nPass the sources to match these against the design.");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
    let Some(design) = design else {
        report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
        return Ok(ExitCode::FAILURE);
    };

    let flat = rtlscope_analyse::flat::flatten(&design);
    let report = rtlscope_wave::match_signals(&dump_file, &design, &flat, prefix.as_deref());

    let suggestions = match &module {
        Some(name) => {
            let Some((module_id, _)) = design.module_by_name(name) else {
                let known: Vec<&str> = design.modules.iter().map(|m| m.name.as_str()).collect();
                anyhow::bail!("no module `{name}` in the design; have {known:?}");
            };
            rtlscope_wave::bind::suggest(&design, module_id, &instance)
        }
        None => Vec::new(),
    };

    if json {
        let value = serde_json::json!({ "matching": &report, "buses": &suggestions });
        let text = if pretty {
            serde_json::to_string_pretty(&value)?
        } else {
            serde_json::to_string(&value)?
        };
        println!("{text}");
    } else {
        print!("{}", cmd::wave::info_text(&report, variables));
        if module.is_some() {
            println!();
            print!("{}", cmd::wave::suggestions_text(&suggestions, &report.prefix));
        }
    }

    report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
    Ok(exit_code(&diags.1, sources.deny_warnings))
}

/// Measures over a recording what the structure already counted.
///
/// The clock is the road's own when the structure found one, because that is
/// the clock the stages were counted in and cycles counted in another would not
/// be the same unit.
fn measure_depth(
    dump: &std::path::Path,
    design: &rtlscope_ir::Design,
    report: &rtlscope_analyse::DepthReport,
    prefix: Option<String>,
    clock: Option<String>,
    from: &str,
    to: &str,
) -> anyhow::Result<rtlscope_wave::LatencyReport> {
    let mut dump_file = rtlscope_wave::Dump::open(dump)?;
    let flat = rtlscope_analyse::flat::flatten(design);
    let matches = rtlscope_wave::match_signals(&dump_file, design, &flat, prefix.as_deref());

    let named = clock.or_else(|| report.clock.clone());
    let Some(named) = named else {
        anyhow::bail!(
            "no clock was crossed between these two, so there are no cycles to measure in.               Pass --clock to count in one anyway."
        );
    };
    let cycles = rtlscope_wave::stages::cycles(&mut dump_file, &matches, &named)?;
    Ok(rtlscope_wave::latency::latency(&mut dump_file, &matches, &cycles, from, to)?)
}

/// Reads a protocol off a dump.
fn run_decode(
    dump: Option<PathBuf>,
    protocol: Option<String>,
    maps: Vec<String>,
    list: bool,
    json: bool,
    pretty: bool,
) -> anyhow::Result<ExitCode> {
    if list {
        print!("{}", cmd::wave::protocols_text());
        return Ok(ExitCode::SUCCESS);
    }
    let Some(protocol) = protocol else {
        anyhow::bail!("pass --protocol NAME, or --list to see them");
    };
    let Some(dump) = dump else {
        anyhow::bail!("pass the dump to read");
    };
    let Some(decoder) = rtlscope_wave::decode::by_name(&protocol) else {
        let known: Vec<&'static str> =
            rtlscope_wave::decode::all().iter().map(|d| d.protocol()).collect();
        anyhow::bail!("no protocol `{protocol}`; there is {}", known.join(", "));
    };

    let bindings: Vec<rtlscope_wave::Binding> =
        maps.iter().map(|text| rtlscope_wave::Binding::parse(text)).collect::<Result<_, _>>()?;

    let mut dump_file = rtlscope_wave::Dump::open(&dump)?;
    let resolved =
        rtlscope_wave::ResolvedBindings::resolve(&mut dump_file, decoder.channels(), &bindings)?;
    let report = decoder.decode(&dump_file, &resolved);

    if json {
        let text = if pretty {
            serde_json::to_string_pretty(&report)?
        } else {
            serde_json::to_string(&report)?
        };
        println!("{text}");
    } else {
        print!("{}", cmd::wave::decode_text(&report));
    }

    // Anything the decoder called an error is worth a non-zero exit, so this
    // can gate a build.
    Ok(if report.errors() > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

/// The pipeline's stages against the dump's cycles.
#[allow(clippy::too_many_arguments)]
fn run_stages(
    dump: PathBuf,
    sources: SourceArgs,
    top: Option<String>,
    prefix: Option<String>,
    clock: Option<String>,
    first: usize,
    count: usize,
    valids: Vec<String>,
    values: Vec<String>,
    json: bool,
    pretty: bool,
) -> anyhow::Result<ExitCode> {
    let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
    let Some(design) = design else {
        report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
        return Ok(ExitCode::FAILURE);
    };

    let mut dump_file = rtlscope_wave::Dump::open(&dump)?;
    let flat = rtlscope_analyse::flat::flatten(&design);
    let matches = rtlscope_wave::match_signals(&dump_file, &design, &flat, prefix.as_deref());

    let report = rtlscope_analyse::pipeline::analyse(&design);
    if report.domains.is_empty() {
        anyhow::bail!("nothing in this design is clocked, so it has no stages to lay out");
    }

    // The domains come deepest first, so without a `--clock` the one drawn is
    // the deepest that the dump actually recorded.
    let chosen = match &clock {
        Some(name) => report
            .domains
            .iter()
            .find(|d| d.clock == *name || d.clock.rsplit('.').next() == Some(name.as_str())),
        None => report.domains.iter().find(|d| matches.by_ir_name(&d.clock).is_some()),
    };
    let Some(domain) = chosen else {
        let known: Vec<&str> = report.domains.iter().map(|d| d.clock.as_str()).collect();
        anyhow::bail!(
            "no clock domain of this design was found in the dump; the design has {known:?}.              Pass --clock to name one, or --prefix if the scope was inferred wrongly."
        );
    };

    let mut layout = rtlscope_wave::Layout::window(first, count);
    layout.valid = parse_stage_pairs(&valids, "--valid")?;
    layout.payload = parse_stage_pairs(&values, "--value")?;

    let cycles = rtlscope_wave::stages::cycles(&mut dump_file, &matches, &domain.clock)?;
    let view = rtlscope_wave::stages::occupancy(&mut dump_file, &matches, domain, &cycles, &layout);

    if json {
        let text = if pretty {
            serde_json::to_string_pretty(&view)?
        } else {
            serde_json::to_string(&view)?
        };
        println!("{text}");
    } else {
        print!("{}", cmd::wave::stages_text(&view));
    }

    report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
    Ok(exit_code(&diags.1, sources.deny_warnings))
}

/// What a testbench run came to, and where in a dump each test sits.
/// Two recordings, held against each other.
fn run_wave_diff(
    dump: PathBuf,
    against: PathBuf,
    json: bool,
    pretty: bool,
) -> anyhow::Result<ExitCode> {
    let mut a = rtlscope_wave::Dump::open(&dump)?;
    let mut b = rtlscope_wave::Dump::open(&against)?;
    let report = rtlscope_wave::compare(&mut a, &mut b);

    let name = |path: &PathBuf| path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let (left, right) = (name(&dump), name(&against));
    if json {
        let value = serde_json::json!({
            "a": left,
            "b": right,
            "shared": report.shared,
            "only_in_a": report.only_in_a,
            "only_in_b": report.only_in_b,
            "differing": report.differing,
            "problems": report.problems,
        });
        let text = match pretty {
            true => serde_json::to_string_pretty(&value)?,
            false => serde_json::to_string(&value)?,
        };
        println!("{text}");
    } else {
        print!("{}", cmd::wave::compare_text(&report, &left, &right));
    }

    // Non-zero when they part, so a regression can gate a build. A signal only
    // one of them has is not that: the two may simply have been dumped with
    // different settings, which is the reader's business and not a failure.
    Ok(match report.agrees() {
        true => ExitCode::SUCCESS,
        false => ExitCode::FAILURE,
    })
}

fn run_tb_results(
    results: PathBuf,
    dump: Option<PathBuf>,
    json: bool,
    pretty: bool,
) -> anyhow::Result<ExitCode> {
    let run = rtlscope_tb::results::read(&results)?;

    let opened = match &dump {
        Some(path) => Some((
            rtlscope_wave::Dump::open(path)?,
            path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        )),
        None => None,
    };

    if json {
        let value = serde_json::json!({
            "tests": &run.tests,
            "problems": &run.problems,
            "basis": rtlscope_tb::results::BASIS,
            "ticks": opened.as_ref().map(|(dump, _)| {
                run.tests.iter().map(|test| dump.ticks_of_ns(test.end_ns)).collect::<Vec<_>>()
            }),
        });
        let text = if pretty {
            serde_json::to_string_pretty(&value)?
        } else {
            serde_json::to_string(&value)?
        };
        println!("{text}");
    } else {
        let (dump_file, name) = match &opened {
            Some((dump, name)) => (Some(dump), name.as_str()),
            None => (None, ""),
        };
        print!("{}", cmd::tb::results_text(&run, dump_file, name));
    }

    // A failing test is worth a non-zero exit, so this can gate a build.
    Ok(if run.counts().1 > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

/// Builds and runs a testbench the reader wrote.
///
/// The design is still elaborated first, and still has to be a design: this
/// reports on the same sources every other command reads, and a testbench that
/// runs against sources RTLScope could not make sense of would be a waveform
/// with nothing to read it against.
fn run_written_testbench(
    bench: &Bench,
    sources: &[PathBuf],
    module: &str,
    engine: SimEngine,
    out_dir: Option<PathBuf>,
    verbose: bool,
    ignored: (u64, u32, bool, bool),
) -> anyhow::Result<ExitCode> {
    let files: Vec<PathBuf> = bench
        .files
        .iter()
        .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect();
    let (uir, _) = rtlscope_sv::lower_files(&files, &rtlscope_sv::ParseOptions::default());
    let read = rtlscope_tb::bench::read(&files, &uir, bench.top.as_deref())?;
    for note in &read.notes {
        eprintln!("note:  {note}");
    }
    // Said rather than silently dropped. Somebody who passed `--cycles 5000`
    // and got a run of a different length should be told which of the two
    // decided it, not left to work it out from the waveform.
    let (cycles, seed, drawn, tuned) = ignored;
    if drawn {
        eprintln!("note:  `--pattern` is ignored: your testbench decides the stimulus");
    }
    if tuned {
        eprintln!("note:  `--override` and `--clock` are ignored: your testbench sets those");
    }
    let _ = (cycles, seed);

    let work = out_dir.unwrap_or_else(|| PathBuf::from("rtlscope-sim").join(&read.top));
    eprintln!("running `{}` on {} with {}…", read.top, module, engine_name(engine));
    let outcome = rtlscope_tb::bench::simulate(
        engine.into(),
        &work,
        &read,
        sources,
        &rtlscope_tb::Tools::default(),
    )?;
    if verbose {
        eprint!("{}", outcome.log);
    }
    println!("{}", outcome.dump.display());
    Ok(ExitCode::SUCCESS)
}

/// A testbench the reader wrote, as the command line spells it.
struct Bench {
    files: Vec<PathBuf>,
    top: Option<String>,
}

/// Simulates a module and says where the waveform is.
#[allow(clippy::too_many_arguments)]
fn run_sim(
    module: String,
    sources: SourceArgs,
    top: Option<String>,
    engine: SimEngine,
    cycles: u64,
    seed: u32,
    pattern: Option<PathBuf>,
    python: Option<PathBuf>,
    overrides: Vec<String>,
    clocks: Vec<String>,
    bench: Bench,
    out_dir: Option<PathBuf>,
    verbose: bool,
) -> anyhow::Result<ExitCode> {
    let drawn = pattern.as_deref().map(read_pattern).transpose()?;
    // What the simulator is handed: the SystemVerilog, which for a Veryl
    // project is what Veryl wrote rather than what the reader named.
    let paths = sources.expand()?.sources;
    let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
    let Some(design) = design else {
        report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
        return Ok(ExitCode::FAILURE);
    };

    let Some((module_id, _)) = design.module_by_name(&module) else {
        let known: Vec<&str> = design.modules.iter().map(|m| m.name.as_str()).collect();
        anyhow::bail!("no module `{module}` in the design; have {known:?}");
    };

    // Absolute, because the simulator is run from its own directory.
    let absolute: Vec<PathBuf> = paths
        .iter()
        .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect();

    // A testbench the reader wrote takes over from here: it decides the
    // stimulus, the cycles and when to stop, so nothing is generated and none
    // of the knobs for a generated harness apply.
    if !bench.files.is_empty() {
        return run_written_testbench(
            &bench,
            &absolute,
            &module,
            engine,
            out_dir,
            verbose,
            (cycles, seed, drawn.is_some(), !overrides.is_empty() || !clocks.is_empty()),
        );
    }
    // A drawing says how long it is, and running fewer cycles than it has
    // columns would leave part of it unplayed without saying so.
    let cycles = match &drawn {
        Some(pattern) => cycles.max(pattern.cycles + 4),
        None => cycles,
    };
    let options = rtlscope_tb::TbOptions {
        cycles,
        overrides: parse_pairs(&overrides, "--override")?,
        periods: parse_pairs(&clocks, "--clock")?
            .into_iter()
            .map(|(name, value)| (name, value as u32))
            .collect(),
        sources: absolute.iter().map(|path| path.display().to_string()).collect(),
        dump_scope: None,
        stimulus: match drawn.clone() {
            Some(pattern) => rtlscope_tb::Stimulus::Drawn(Box::new(pattern)),
            // Without a drawing: an untouched harness ties its inputs off and
            // nothing flows, which makes a dump with nothing in it.
            None => rtlscope_tb::Stimulus::Random { seed },
        },
    };

    let base = design.modules[module_id].base_name.clone();
    // A drawing is played by cocotb, which reads it as data; random stimulus
    // goes straight to a simulator, which needs no Python at all.
    let flavor = match drawn {
        Some(_) => rtlscope_tb::Flavor::Cocotb,
        None => rtlscope_tb::Flavor::Sv,
    };
    let generated = rtlscope_tb::generate_with(&design, module_id, &options, flavor)?;
    for note in &generated.notes {
        eprintln!("note:  {note}");
    }

    // A command line comes with a shell's PATH, so the simulator is found the
    // ordinary way and there is nothing here to say about it. What this adds is
    // the design's own directory as somewhere to look for a `.venv-cocotb`, so
    // `--pattern` works from anywhere under a checkout rather than only from
    // the one directory the venv happens to sit in.
    let tools =
        rtlscope_tb::Tools { python, ..rtlscope_tb::Tools::default() }.near_sources(&absolute);

    let work = out_dir.unwrap_or_else(|| PathBuf::from("rtlscope-sim").join(&base));
    let outcome = match flavor {
        rtlscope_tb::Flavor::Cocotb => {
            eprintln!("playing the pattern on `{base}` with cocotb…");
            rtlscope_tb::run::play(&work, &base, &generated.files, &tools, engine.into())?
        }
        rtlscope_tb::Flavor::Sv => {
            eprintln!("running {} on `{base}` for {cycles} cycle(s)…", engine_name(engine));
            rtlscope_tb::simulate(engine.into(), &work, &base, &generated.files, &absolute, &tools)?
        }
    };

    if verbose {
        eprint!("{}", outcome.log);
    }

    // What came out, measured against the design — the same question
    // `wave-info` answers, asked here so a bad harness cannot look like a good
    // run.
    let dump = rtlscope_wave::Dump::open(&outcome.dump)?;
    let flat = rtlscope_analyse::flat::flatten(&design);
    let matched = rtlscope_wave::match_signals(&dump, &design, &flat, None);

    // What the drawing said should happen, and whether it did. Reported before
    // the dump so the answer is the first thing read.
    let mut failed = false;
    if drawn.is_some() {
        let verdict_at = work.join(format!("{base}_verdict.json"));
        match rtlscope_tb::Verdict::read(&verdict_at) {
            Ok(verdict) => {
                eprintln!("\n{}", verdict.summary());
                for miss in &verdict.failures {
                    eprintln!(
                        "  column {:<5} {:<20} expected {:<10} got {}",
                        miss.cycle, miss.port, miss.expected, miss.got
                    );
                }
                failed = !verdict.passed();
            }
            // The run happened; only the verdict is missing. Saying so beats
            // reporting a pass that was never checked.
            Err(why) => eprintln!("\nthe run left no verdict: {why}"),
        }
    }

    println!("{}", outcome.dump.display());
    eprintln!(
        "\n{} of {} signal(s) matched; the dump ends at {}",
        matched.matched.len(),
        matched.prefix_score.1,
        dump.max_time()
    );
    eprintln!("\nlook at it with:");
    eprintln!("  rtlscope stages {} <sources> --top {}", outcome.dump.display(), base);
    eprintln!("  rtlscope-gui <sources> --top {base} --dump {}", outcome.dump.display());

    report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
    if failed {
        // A drawing that did not hold is the answer, and a shell has to be
        // able to see it.
        return Ok(ExitCode::FAILURE);
    }
    Ok(exit_code(&diags.1, sources.deny_warnings))
}

/// Reads a drawn pattern, saying which file was wrong when it is.
fn read_pattern(path: &std::path::Path) -> anyhow::Result<rtlscope_tb::Pattern> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the pattern {}", path.display()))?;
    rtlscope_tb::Pattern::from_json(&text)
        .map_err(|why| anyhow::anyhow!("{} is not a pattern: {why}", path.display()))
}

fn engine_name(engine: SimEngine) -> &'static str {
    match engine {
        SimEngine::Verilator => "verilator",
        SimEngine::Icarus => "icarus",
    }
}

/// Checks the IR against a Yosys netlist of the same sources.
fn run_yosys_check(
    netlist: PathBuf,
    sources: SourceArgs,
    top: Option<String>,
    json: bool,
    pretty: bool,
) -> anyhow::Result<ExitCode> {
    let paths = sources.expand()?.sources;
    let (design, diags) = elaborate_sources(&sources, top.as_deref())?;
    let Some(design) = design else {
        report_diagnostics(&diags.1, &diags.0, sources.diag_format)?;
        return Ok(ExitCode::FAILURE);
    };

    // A missing netlist is the usual first run, so say how to make one rather
    // than only that it is not there.
    if !netlist.exists() {
        let files: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        anyhow::bail!(
            "no netlist at `{}`. Produce it with:\n\n  {}\n",
            netlist.display(),
            cmd::yosys::yosys_command(
                &files,
                &design.top_module().base_name,
                &netlist.display().to_string()
            )
        );
    }

    let read = rtlscope_yosys::read(&netlist)?;
    let report = rtlscope_yosys::check(&design, &read);

    if json {
        let text = if pretty {
            serde_json::to_string_pretty(&report)?
        } else {
            serde_json::to_string(&report)?
        };
        println!("{text}");
    } else {
        print!("{}", cmd::yosys::check_text(&report));
    }

    report_diagnostics(&diags.1, &design.files, sources.diag_format)?;
    // A disagreement between two front ends is worth a non-zero exit: one of
    // them is wrong, and it is usually this one.
    Ok(if report.agrees() { exit_code(&diags.1, sources.deny_warnings) } else { ExitCode::FAILURE })
}

fn run_diagram(
    sources: SourceArgs,
    top: Option<String>,
    module: Option<String>,
    out: Option<PathBuf>,
    show_clocks: bool,
) -> anyhow::Result<ExitCode> {
    let lowered = sources.lowered()?;
    let (uir, mut diags) = (lowered.uir, lowered.diags);
    let (design, elab_diags) = rtlscope_elab::elaborate(&uir, top.as_deref());
    diags.extend(elab_diags);

    let Some(design) = design else {
        report_diagnostics(&diags, &uir.files, sources.diag_format)?;
        return Ok(ExitCode::FAILURE);
    };

    let module_id = match &module {
        None => design.top,
        Some(name) => match design.module_by_name(name) {
            Some((id, _)) => id,
            None => {
                let known: Vec<&str> = design.modules.iter().map(|m| m.name.as_str()).collect();
                anyhow::bail!("no module `{name}` in the design; have {known:?}");
            }
        },
    };

    let geom = rtlscope_graph::diagram(&design, module_id);
    let svg =
        rtlscope_graph::render(&geom, &design.files, &rtlscope_graph::SvgOptions { show_clocks });

    match &out {
        Some(path) => {
            std::fs::write(path, &svg).with_context(|| format!("writing {}", path.display()))?;
            eprintln!(
                "{}: {} boxes, {} wires -> {}",
                geom.module,
                geom.boxes.len(),
                geom.wires.len(),
                path.display()
            );
        }
        None => println!("{svg}"),
    }

    report_diagnostics(&diags, &design.files, sources.diag_format)?;
    Ok(exit_code(&diags, sources.deny_warnings))
}

fn report_diagnostics(
    diags: &Diagnostics,
    files: &rtlscope_ir::FileTable,
    format: DiagFormat,
) -> anyhow::Result<()> {
    if diags.is_empty() {
        return Ok(());
    }
    match format {
        DiagFormat::Text => {
            eprint!("{}", diags.render(files));
            eprintln!(
                "{} error(s), {} warning(s)",
                diags.count(Severity::Error),
                diags.count(Severity::Warning)
            );
        }
        DiagFormat::Json => eprintln!("{}", serde_json::to_string(diags)?),
    }
    Ok(())
}

fn exit_code(diags: &Diagnostics, deny_warnings: bool) -> ExitCode {
    let failed = diags.has_errors() || (deny_warnings && diags.count(Severity::Warning) > 0);
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
