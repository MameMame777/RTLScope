//! RTLScope as an MCP server.
//!
//! The same views the CLI prints, answerable in a conversation. Nothing here
//! analyses anything: every tool parses, elaborates, and then hands the work to
//! the crate that already does it, so a question asked here and the same
//! question asked at the terminal cannot drift apart.
//!
//! Two things shape the design.
//!
//! **Reading a design is expensive and repeated.** The 41-file design this was
//! built against takes about six seconds to parse and elaborate, and a
//! conversation asks four or five questions of the same sources in a row. So
//! the elaborated design is cached against the files it came from and their
//! modification times: ask again and the answer is immediate, edit a file and
//! the next question re-reads it.
//!
//! **An answer has to fit in a reply.** A whole elaborated design is megabytes
//! of JSON; pasting that into a conversation helps nobody. So the tools are
//! shaped like questions rather than like dumps — `modules` lists what is
//! there, `module` describes one of them — and the ones that could still run
//! long say how much they left out rather than truncating in silence.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, ServiceExt as _, tool, tool_handler, tool_router};
use rtlscope_analyse::{cdc, fsm, lint, pipeline};
use rtlscope_ir::{Design, Diagnostics};
use rtlscope_sv::ParseOptions;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ------------------------------------------------------------ arguments ---

/// Where the RTL is, which every tool needs before it can answer anything.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct Sources {
    /// Absolute paths to the SystemVerilog or Verilog files to read. A
    /// directory is not expanded — pass the files. Veryl is read too: a
    /// `.veryl` file, a `Veryl.toml`, or a Veryl project directory stands for
    /// the whole project, which is built with `veryl build` and read as the
    /// SystemVerilog it wrote; every location answered still points into the
    /// `.veryl`.
    ///
    /// Leave it out to use the design open in RTLScope's window, if one is. The
    /// answer from `modules` names which design that turned out to be.
    #[serde(default)]
    files: Vec<String>,
    /// The top module. Leave it out when exactly one module is instantiated by
    /// nothing else, which is the usual case.
    #[serde(default)]
    top: Option<String>,
    /// Preprocessor defines, as `NAME` or `NAME=VALUE`. A design with `ifdef`
    /// regions reads differently without them.
    #[serde(default)]
    defines: Vec<String>,
    /// Directories to search for `` `include ``.
    #[serde(default)]
    includes: Vec<String>,
}

/// What reaches a signal, or what it reaches.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ConeQuery {
    #[serde(flatten)]
    sources: Sources,
    /// The signal to ask about. Full path (`u_dsp.u_fir.acc`) or the last part
    /// alone (`acc`) when only one signal wears it.
    signal: String,
    /// Walk downstream — what this signal decides — rather than upstream.
    #[serde(default)]
    loads: bool,
    /// How many hops to follow. Three by default, which reaches through two
    /// registers and the logic between them.
    #[serde(default)]
    depth: Option<usize>,
}

/// The whole design's depth, or the distance between two of its signals.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct PipelineQuery {
    #[serde(flatten)]
    sources: Sources,
    /// The signal a value leaves from. Give it with `to` to ask how many
    /// clocks lie between two points instead of how deep the design is.
    ///
    /// Full path (`u_dsp.u_fir.acc`) or the last part alone (`acc`) when only
    /// one signal wears it. `signals` lists what there is.
    #[serde(default)]
    from: Option<String>,
    /// The signal it arrives at.
    #[serde(default)]
    to: Option<String>,
}

/// How long a value took, measured over a recording.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct LatencyQuery {
    #[serde(flatten)]
    sources: Sources,
    /// Absolute path to the `.vcd` or `.fst` to measure over.
    dump: String,
    /// The signal a value leaves from.
    from: String,
    /// The signal it arrives at.
    to: String,
    /// The scope the design sits under in the dump. Inferred by counting when
    /// left out.
    #[serde(default)]
    prefix: Option<String>,
    /// Which clock's edges to count cycles in. The road's own, by default.
    #[serde(default)]
    clock: Option<String>,
}

/// The names a design knows its signals by.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct SignalsQuery {
    #[serde(flatten)]
    sources: Sources,
    /// Only names containing this, ignoring case. Left out, everything.
    #[serde(default)]
    pattern: Option<String>,
}

/// One signal, in one module.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct DriverQuery {
    #[serde(flatten)]
    sources: Sources,
    /// The module the signal is in, as `modules` names it.
    module: String,
    /// The signal, by the name the source gave it inside that module.
    signal: String,
}

/// A waveform dump on its own.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct DumpQuery {
    /// Absolute path to the VCD or FST to read.
    dump: String,
}

/// A waveform dump, and how to read a protocol off it.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct DecodeQuery {
    /// Absolute path to the VCD or FST to read.
    dump: String,
    /// Which protocol: `pixel`, `axis`, `axi4` or `i2c`. Call `protocols` to
    /// see what each one needs.
    protocol: String,
    /// One per channel, as `role=signal.path`, or `role=!signal.path` for a
    /// signal recorded inverted — which is how an open-drain bus is modelled.
    /// Call `buses` to have these worked out from the design.
    map: Vec<String>,
}

/// Which buses a module has, worked out from its signal names.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct BusQuery {
    #[serde(flatten)]
    sources: Sources,
    /// The module to look at, as `modules` names it.
    module: String,
    /// Where that module sits in the dump, e.g. `tb.dut.u_rx`. Leave it out if
    /// the dump starts at the module itself.
    #[serde(default)]
    instance: String,
}

/// A design and a dump of it, laid out cycle by cycle.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct StageQuery {
    #[serde(flatten)]
    sources: Sources,
    /// Absolute path to the VCD or FST to read.
    dump: String,
    /// The scope the design sits under in the dump, e.g. `tb.dut`. Worked out
    /// by counting when left out.
    #[serde(default)]
    prefix: Option<String>,
    /// Which clock domain, as `pipeline_depth` names its clock. Without it, the
    /// deepest domain the dump actually recorded.
    #[serde(default)]
    clock: Option<String>,
    /// The first cycle to report. Cycle 0 is the clock's first rising edge in
    /// the dump.
    #[serde(default)]
    from: usize,
    /// How many cycles. 80 by default, and capped, because a grid of thousands
    /// of cells is not an answer anyone reads.
    #[serde(default)]
    count: Option<usize>,
    /// Name a stage's valid bit rather than letting it be guessed at, as
    /// `2=g_conv.v3`. Read each row's `basis` first: a row that fell back to
    /// showing movement is the one worth naming.
    #[serde(default)]
    valid: Vec<String>,
    /// Name the register whose value fills a stage's cells, as
    /// `2=g_conv.center3`. Whether a cell reads as a stall is decided by
    /// whether this repeated.
    #[serde(default)]
    value: Vec<String>,
}

/// A design, and a Yosys netlist of the same files.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct YosysQuery {
    #[serde(flatten)]
    sources: Sources,
    /// Absolute path to the JSON `write_json` produced.
    netlist: String,
}

/// What a testbench run came to.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ResultsQuery {
    /// Absolute path to the `results.xml` the run wrote.
    results: String,
    /// Absolute path to the dump those tests produced. With it, each test's
    /// moment comes back as a tick of that dump as well as in nanoseconds.
    #[serde(default)]
    dump: Option<String>,
}

/// One module of a design.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ModuleQuery {
    #[serde(flatten)]
    sources: Sources,
    /// The module's name as `modules` reports it. A module built more than once
    /// with different parameters has a name per specialisation, like
    /// `fifo$DEPTH=16`.
    module: String,
}

// -------------------------------------------------------------- answers ---

#[derive(Debug, Serialize, JsonSchema)]
struct Overview {
    /// Which design this is about. Present only when the query named no files
    /// and the design open in RTLScope was used instead — the one case where
    /// what was answered is not literally what was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    top: String,
    modules: Vec<ModuleSummary>,
    /// The instance tree, one line per instance, `parent.child : module`.
    hierarchy: Vec<String>,
    errors: usize,
    warnings: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ModuleSummary {
    name: String,
    /// The name before parameter specialisation. Two entries can share it.
    base_name: String,
    /// The name the author wrote, when a tool rewrote it on the way to
    /// SystemVerilog — `Control` for a Veryl module read as `lights_Control`.
    /// Either name is accepted wherever a module is named.
    #[serde(skip_serializing_if = "Option::is_none")]
    written: Option<String>,
    location: String,
    ports: usize,
    nets: usize,
    instances: usize,
    processes: usize,
    /// True when only a header was found — an IP with no source. Its contents
    /// are unknown rather than empty.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    blackbox: bool,
    /// Constructs inside it that RTLScope could not model.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct WaveSummary {
    /// One tick of the time axis, e.g. "1 ns".
    tick: String,
    end_time: u64,
    variables: usize,
    /// Every signal, by the path it was recorded under. Long lists are cut and
    /// `omitted` says by how much.
    signals: Vec<String>,
    omitted: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
struct Diagram {
    module: String,
    svg: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct DiagnosticList {
    errors: usize,
    warnings: usize,
    /// One line each, worst first, capped — `omitted` says how many did not fit.
    reported: Vec<String>,
    omitted: usize,
}

// ---------------------------------------------------------------- server ---

#[derive(Debug, Clone)]
struct Server {
    cache: std::sync::Arc<Mutex<Option<Cached>>>,
}

/// The last design read, and what it was read from.
struct Cached {
    key: CacheKey,
    design: Design,
    diags: Diagnostics,
}

#[derive(PartialEq, Eq)]
struct CacheKey {
    files: Vec<(PathBuf, Option<SystemTime>)>,
    top: Option<String>,
    defines: Vec<String>,
    includes: Vec<String>,
}

impl std::fmt::Debug for Cached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cached")
    }
}

/// How many diagnostics one answer carries.
const MAX_DIAGNOSTICS: usize = 60;

/// How many register names one pipeline stage lists.
const MAX_STAGE_NAMES: usize = 24;

/// How many of a dump's signals one answer names.
const MAX_SIGNALS: usize = 400;

/// How many annotations come back from a decode.
const MAX_ANNOTATIONS: usize = 400;

/// How many cycles of a stage diagram to report, and how many cells before the
/// value written in each one is dropped.
const MAX_CYCLES: usize = 192;
const MAX_CELLS: usize = 4_000;

#[tool_router]
impl Server {
    fn new() -> Self {
        Self { cache: std::sync::Arc::new(Mutex::new(None)) }
    }

    /// What is in this design: every module, its size, and the instance tree.
    ///
    /// Start here. The module names it reports are the ones the other tools
    /// take, including the `name$PARAM=value` form a module specialised more
    /// than once is given.
    #[tool(
        name = "modules",
        description = "\
List the modules in a design, their sizes, and the instance tree. Start here: \
the names reported are the ones every other tool takes, and — when `files` was \
left out — the `source` field says which design was read."
    )]
    async fn modules(
        &self,
        Parameters(sources): Parameters<Sources>,
    ) -> Result<Json<Overview>, ErrorData> {
        let source = Server::whose(&sources);
        self.with_design(sources, move |design, diags| {
            let modules = design
                .modules
                .iter()
                .map(|module| ModuleSummary {
                    name: module.name.clone(),
                    base_name: module.base_name.clone(),
                    written: module.written.clone(),
                    location: design.files.render(module.span),
                    ports: module.ports.len(),
                    nets: module.nets.len(),
                    instances: module.insts.len(),
                    processes: module.procs.len(),
                    blackbox: module.is_blackbox,
                    skipped: module
                        .skipped
                        .iter()
                        .map(|s| format!("{} at {}", s.construct, design.files.render(s.span)))
                        .collect(),
                })
                .collect();

            let mut hierarchy = Vec::new();
            walk(design, design.top, "", &mut hierarchy, 0);

            Overview {
                source,
                top: design.modules[design.top].name.clone(),
                modules,
                hierarchy,
                errors: diags.iter().filter(|d| d.severity == rtlscope_ir::Severity::Error).count(),
                warnings: diags
                    .iter()
                    .filter(|d| d.severity == rtlscope_ir::Severity::Warning)
                    .count(),
            }
        })
        .await
        .map(Json)
    }

    /// One module in full: parameters, ports, nets, instances and processes.
    ///
    /// The same shape `rtlscope dump-ir` prints, narrowed to one module so the
    /// answer fits in a reply.
    #[tool(
        name = "module",
        description = "\
Describe one module: its parameters with their evaluated values, its ports and \
widths, its nets, its instances with resolved connections, and its processes \
with the clock each runs on and the nets each reads and writes."
    )]
    async fn module(
        &self,
        Parameters(query): Parameters<ModuleQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let wanted = query.module.clone();
        let report = self
            .with_design(query.sources, move |design, _| {
                let report = rtlscope_cli::cmd::dump_ir::build(design);
                serde_json::to_value(&report).ok().and_then(|value| {
                    let modules = value.get("modules")?.as_array()?;
                    modules
                        .iter()
                        .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(&wanted))
                        .cloned()
                })
            })
            .await?;

        report.map(Json).ok_or_else(|| {
            ErrorData::invalid_params(
                format!("no module `{}` in this design; `modules` lists the names", query.module),
                None,
            )
        })
    }

    /// A block diagram of one module, as SVG.
    #[tool(
        name = "diagram",
        description = "\
Draw one module as an SVG block diagram: its instances as boxes, its ports on \
the edges, and the nets between them as wires. Every box and wire carries the \
source location it came from."
    )]
    async fn diagram(
        &self,
        Parameters(query): Parameters<ModuleQuery>,
    ) -> Result<Json<Diagram>, ErrorData> {
        let wanted = query.module.clone();
        let svg = self
            .with_design(query.sources, move |design, _| {
                let (id, _) = design.module_by_name(&wanted)?;
                let geom = rtlscope_graph::diagram(design, id);
                Some(rtlscope_graph::render(
                    &geom,
                    &design.files,
                    &rtlscope_graph::SvgOptions::default(),
                ))
            })
            .await?;

        svg.map(|svg| Json(Diagram { module: query.module.clone(), svg })).ok_or_else(|| {
            ErrorData::invalid_params(
                format!("no module `{}` in this design; `modules` lists the names", query.module),
                None,
            )
        })
    }

    /// The state machines, with their states named as the source named them.
    #[tool(
        name = "state_machines",
        description = "\
Find the state machines: their states, the transitions between them and the \
condition on each. A state machine here is a register whose next value a `case` \
on itself decides — both the one-process and two-process styles. State names \
come from the enum or the localparams the source used, so they read as `ST_ACK` \
rather than as `2'd3`. Also reports states nothing enters and states nothing \
leaves."
    )]
    async fn state_machines(
        &self,
        Parameters(sources): Parameters<Sources>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        self.with_design(sources, |design, _| json(fsm::find(design))).await.map(Json)
    }

    /// The clock domains and what crosses between them.
    #[tool(
        name = "clock_domains",
        description = "\
List the clock domains and every signal that crosses between them. The design \
is flattened first, so a crossing between two instances is found even when both \
call their clock `clk`. Only the two-flop synchroniser is recognised: an async \
FIFO, a gray-coded pointer or a handshake is correct and is still listed, so \
the list is what to check rather than what is wrong."
    )]
    async fn clock_domains(
        &self,
        Parameters(sources): Parameters<Sources>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        self.with_design(sources, |design, _| json(cdc::analyse(design))).await.map(Json)
    }

    /// Where a signal's value comes from.
    #[tool(
        name = "drivers",
        description = "\
Say where a signal's value comes from: what drives it — a clocked process, \
combinational logic, a port of the module, the output of a child instance, or \
nothing at all — and what decides what that driver writes. Dependencies are \
split into the data on the right-hand side and the conditions on the way to it, \
each quoted as the source wrote it, so `en` reads as `if (en)` rather than as a \
name to guess about.\n\n\
One hop, not the whole chain: follow it by asking again about one of the \
signals that came back. Reach for this when a waveform shows a value and the \
question is why — `read problems` first, since a bit slice, a constant tied to \
a port, or more than one driver are all things this reports rather than \
resolves."
    )]
    async fn drivers(
        &self,
        Parameters(query): Parameters<DriverQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let (module, signal) = (query.module.clone(), query.signal.clone());
        self.with_design(query.sources, move |design, _| {
            let Some((module_id, found)) = design.module_by_name(&module) else {
                return Err(ErrorData::invalid_params(
                    format!("no module `{module}`; `modules` lists the names"),
                    None,
                ));
            };
            let Some((net, _)) = found.nets.iter_enumerated().find(|(_, net)| net.name == signal)
            else {
                let known: Vec<&str> =
                    found.nets.iter().map(|net| net.name.as_str()).take(40).collect();
                return Err(ErrorData::invalid_params(
                    format!("no signal `{signal}` in `{module}`; it has {known:?}"),
                    None,
                ));
            };
            let flat = rtlscope_analyse::flat::flatten(design);
            let traced = rtlscope_analyse::drive::trace(design, &flat, module_id, net);
            Ok(Json(json(described(design, &traced))))
        })
        .await?
    }

    /// Inferred latches and signals nothing reads.
    #[tool(
        name = "lint",
        description = "\
Report inferred latches, signals driven and read by nobody, and modules \
instantiated by nothing. A latch is combinational logic that does not assign a \
signal on every path through it, which makes synthesis build storage to hold \
the old value — almost always a missing `else` or `default`. A process \
containing a construct RTLScope could not model is left alone rather than guessed \
at."
    )]
    async fn lint(
        &self,
        Parameters(sources): Parameters<Sources>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        self.with_design(sources, |design, _| json(lint::analyse(design))).await.map(Json)
    }

    /// What decides a signal, or what it decides.
    #[tool(
        name = "cone",
        description = "\
Report what reaches a signal, or what it reaches: the cone of influence. Ask \
upstream — what decides this value — or set `loads` for downstream, what this \
value disturbs. The answer is levels of signals by distance in hops, plus the \
edges between them, saying for each whether the value crosses a register or \
settles through logic in the same clock. \
\
Clocks and resets are left out of both directions. They decide when a value \
arrives rather than what it is, so following one would reach every register in \
the domain and from there the whole design. \
\
A level wider than two dozen signals is cut and the remainder counted in \
`clipped`, because a level that wide is not an answer anybody reads."
    )]
    async fn cone(
        &self,
        Parameters(query): Parameters<ConeQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let (want, loads) = (query.signal.clone(), query.loads);
        let depth = query.depth.unwrap_or(3);
        self.with_design(query.sources, move |design, _| {
            let flat = rtlscope_analyse::flat::flatten(design);
            let Some(root) = rtlscope_cli::cmd::analyse::signal_named(&flat, design, &want) else {
                return serde_json::json!({
                    "error": format!("no signal called `{want}` in this design"),
                });
            };
            let graph = rtlscope_analyse::depth::signal_graph(design, &flat);
            let cone = match loads {
                true => rtlscope_analyse::cone::fan_out(&graph, root, depth),
                false => rtlscope_analyse::cone::fan_in(&graph, root, depth),
            };
            rtlscope_cli::cmd::analyse::cone_json(&cone, &flat)
        })
        .await
        .map(Json)
    }

    /// How many clocks deep the logic is.
    #[tool(
        name = "pipeline_depth",
        description = "\
Report how many clocks deep the logic is and which registers sit at each depth. \
A stage is a position in the register adjacency graph rather than something the \
source declares, so this finds the shape whether or not it was meant as a \
pipeline, and finds it across module boundaries. Registers that feed each other \
— counters, accumulators, state machines — are reported as feedback rather than \
given a stage number that would mean nothing. \
\
Give `from` and `to` to ask a different question: how many clock edges lie \
between those two signals. The count starts at the first signal's value, so its \
own register does not count, and arriving at a register costs one. Where the \
road loops, or crosses two clocks, or passes a value that decides itself, the \
answer is a reason rather than a number."
    )]
    async fn pipeline_depth(
        &self,
        Parameters(query): Parameters<PipelineQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let (from, to) = (query.from.clone(), query.to.clone());
        if from.is_some() != to.is_some() {
            return Err(ErrorData::invalid_params(
                "`from` and `to` come together: one signal is not a distance. Leave both out \
                 for the whole design's depth."
                    .to_string(),
                None,
            ));
        }
        if let (Some(from), Some(to)) = (from, to) {
            return self
                .with_design(query.sources, move |design, _| {
                    json(rtlscope_analyse::depth::analyse(design, &from, &to))
                })
                .await
                .map(Json);
        }

        self.with_design(query.sources, |design, _| {
            let mut report = pipeline::analyse(design);
            // A stage in a real design can hold a hundred registers, and a
            // hundred names is not an answer anyone reads.
            for domain in &mut report.domains {
                for stage in &mut domain.stages {
                    if stage.registers.len() > MAX_STAGE_NAMES {
                        let hidden = stage.registers.len() - MAX_STAGE_NAMES;
                        stage.registers.truncate(MAX_STAGE_NAMES);
                        stage.registers.push(format!("... and {hidden} more"));
                    }
                }
            }
            json(report)
        })
        .await
        .map(Json)
    }

    /// How long a value took, against how long the structure says it should.
    #[tool(
        name = "path_latency",
        description = "\
Measure over a recording how many clock cycles a value took from one signal to \
another, and hold it against what the structure allows. The structure is exact \
about how many registers lie between two points and says nothing about how long \
they took; a recording says the second and not the first. Both come back, with \
what their disagreement means: measured slower than the structure allows is \
something waiting, and measured faster is a measurement that matched the wrong \
changes. Read `problems` for the beats the pairing could not account for."
    )]
    async fn path_latency(
        &self,
        Parameters(query): Parameters<LatencyQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let dump_path = query.dump.clone();
        let prefix = query.prefix.clone();
        let asked_clock = query.clock.clone();
        let (from, to) = (query.from.clone(), query.to.clone());

        self.with_design(query.sources, move |design, _| {
            let structure = rtlscope_analyse::depth::analyse(design, &from, &to);
            // No road means nothing to measure along. Answering with a
            // histogram under a refusal would invite it to be read as the
            // answer to a question that was refused.
            if structure.failed() {
                return Ok(Json(json(serde_json::json!({
                    "static": structure,
                    "dynamic": serde_json::Value::Null,
                    "cross_check": Vec::<String>::new(),
                }))));
            }

            let mut dump = open_dump(&dump_path)?;
            let flat = rtlscope_analyse::flat::flatten(design);
            let matches = rtlscope_wave::match_signals(&dump, design, &flat, prefix.as_deref());

            let named = asked_clock.or_else(|| structure.clock.clone());
            let Some(named) = named else {
                return Err(ErrorData::invalid_params(
                    "no clock is crossed between these two, so there are no cycles to measure \
                     in. Pass `clock` to count in one anyway."
                        .to_string(),
                    None,
                ));
            };
            let cycles = rtlscope_wave::stages::cycles(&mut dump, &matches, &named)
                .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
            let measured =
                rtlscope_wave::latency::latency(&mut dump, &matches, &cycles, &from, &to)
                    .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;

            let verdict = rtlscope_wave::cross_check(&structure, &measured);
            Ok(Json(json(serde_json::json!({
                "static": structure,
                "dynamic": measured,
                "cross_check": verdict,
            }))))
        })
        .await?
    }

    /// Every name the design knows a signal by.
    #[tool(
        name = "signals",
        description = "\
List the names this design knows its signals by, flattened across the hierarchy \
— `u_dsp.u_fir.acc` rather than `acc` in a module. These are the names \
`pipeline_depth`, `path_latency` and `drivers` take. Pass `pattern` to narrow \
it: a real design has thousands. Signals RTLScope invented for its own use are \
left out, since asking about one would be asking about the tool."
    )]
    async fn signals(
        &self,
        Parameters(query): Parameters<SignalsQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let pattern = query.pattern.clone().map(|text| text.to_lowercase());
        self.with_design(query.sources, move |design, _| {
            let flat = rtlscope_analyse::flat::flatten(design);
            let mut names: Vec<serde_json::Value> = Vec::new();
            let mut total = 0usize;
            for (name, _, width, invented) in flat.all_names(design) {
                if invented {
                    continue;
                }
                if let Some(pattern) = &pattern
                    && !name.to_lowercase().contains(pattern)
                {
                    continue;
                }
                total += 1;
                if names.len() < MAX_SIGNALS {
                    names.push(serde_json::json!({ "name": name, "width": width }));
                }
            }
            let hidden = total.saturating_sub(names.len());
            json(serde_json::json!({
                "signals": names,
                "shown": names.len(),
                "total": total,
                "hidden": hidden,
            }))
        })
        .await
        .map(Json)
    }

    /// The protocols there are decoders for.
    #[tool(
        name = "protocols",
        description = "\
List the protocols that can be read off a waveform, and the channels each one \
needs. Names in brackets are optional: leaving one out narrows what can be said, \
and the decode report says which."
    )]
    async fn protocols(&self) -> Result<Json<serde_json::Value>, ErrorData> {
        let listed: Vec<serde_json::Value> = rtlscope_wave::decode::all()
            .iter()
            .map(|decoder| {
                serde_json::json!({
                    "protocol": decoder.protocol(),
                    "about": decoder.doc(),
                    "channels": decoder.channels().iter().map(|c| serde_json::json!({
                        "role": c.role,
                        "required": c.required,
                        "about": c.doc,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(Json(serde_json::json!(listed)))
    }

    /// What a dump holds.
    #[tool(
        name = "dump_signals",
        description = "\
List what a waveform dump records: its time scale, how long it runs, and the \
signals in it by the paths they were recorded under. Start here when a dump is \
involved, the way `modules` starts a question about a design."
    )]
    async fn dump_signals(
        &self,
        Parameters(query): Parameters<DumpQuery>,
    ) -> Result<Json<WaveSummary>, ErrorData> {
        tokio::task::spawn_blocking(move || {
            let dump = open_dump(&query.dump)?;
            let mut signals: Vec<String> = dump.vars().map(|(name, _)| name.to_string()).collect();
            signals.sort();
            let variables = signals.len();
            let omitted = variables.saturating_sub(MAX_SIGNALS);
            signals.truncate(MAX_SIGNALS);
            let tick =
                dump.timescale().map_or_else(|| "unknown".to_string(), |(f, u)| format!("{f} {u}"));
            Ok(Json(WaveSummary { tick, end_time: dump.max_time(), variables, signals, omitted }))
        })
        .await
        .map_err(|error| {
            ErrorData::internal_error(format!("reading the dump panicked: {error}"), None)
        })?
    }

    /// Which buses a module has, so a decode can be set up without guessing.
    #[tool(
        name = "buses",
        description = "\
Work out which buses a module has, from the shape of its signal names, and \
propose the `map` a decode of each would need. A stream is recognised by its \
handshake — `*_tvalid` with `*_tready`, `*_valid` with `*_pixel` — and an \
open-drain bus is proposed inverted, which is the detail that decides whether \
it decodes at all."
    )]
    async fn buses(
        &self,
        Parameters(query): Parameters<BusQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let module = query.module.clone();
        let instance = query.instance.clone();
        let found = self
            .with_design(query.sources, move |design, _| {
                let (module_id, _) = design.module_by_name(&module)?;
                Some(rtlscope_wave::bind::suggest(design, module_id, &instance))
            })
            .await?;

        found.map(|found| Json(json(found))).ok_or_else(|| {
            ErrorData::invalid_params(
                format!("no module `{}`; `modules` lists the names", query.module),
                None,
            )
        })
    }

    /// Read a protocol off a dump.
    #[tool(
        name = "decode",
        description = "\
Read a protocol off a waveform: transactions, statistics, and everything the \
decoder could not account for. Bind each channel with `role=signal`, or \
`role=!signal` for one recorded inverted. Call `buses` first to have the \
bindings worked out from the design. Annotations are for drawing and are \
returned only up to a limit."
    )]
    async fn decode(
        &self,
        Parameters(query): Parameters<DecodeQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        tokio::task::spawn_blocking(move || {
            let Some(decoder) = rtlscope_wave::decode::by_name(&query.protocol) else {
                let known: Vec<&'static str> =
                    rtlscope_wave::decode::all().iter().map(|d| d.protocol()).collect();
                return Err(ErrorData::invalid_params(
                    format!("no protocol `{}`; there is {}", query.protocol, known.join(", ")),
                    None,
                ));
            };

            let bindings: Vec<rtlscope_wave::Binding> = query
                .map
                .iter()
                .map(|text| rtlscope_wave::Binding::parse(text))
                .collect::<Result<_, _>>()
                .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;

            let mut dump = open_dump(&query.dump)?;
            let resolved =
                rtlscope_wave::ResolvedBindings::resolve(&mut dump, decoder.channels(), &bindings)
                    .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
            let mut report = decoder.decode(&dump, &resolved);

            // Annotations are for a picture, and a picture does not fit here.
            if report.annotations.len() > MAX_ANNOTATIONS {
                let hidden = report.annotations.len() - MAX_ANNOTATIONS;
                report.annotations.truncate(MAX_ANNOTATIONS);
                report.problem(format!(
                    "{hidden} annotation(s) are not listed here; the transactions and \
                     statistics above cover all of them"
                ));
            }
            Ok(Json(json(report)))
        })
        .await
        .map_err(|error| ErrorData::internal_error(format!("decoding panicked: {error}"), None))?
    }

    /// The pipeline laid against the dump's cycles.
    #[tool(
        name = "stage_cycles",
        description = "\
Lay a pipeline's stages against a dump's clock cycles: one row per stage, one \
cell per cycle, saying whether the stage was carrying anything. Occupancy is \
not recorded in a waveform, so each row looks for the valid bit at that stage \
and reports which one it used - or reports that it fell back to showing which \
registers moved, which means something different. Read `basis` on each row \
before reading its cells, and `problems` for what could not be worked out."
    )]
    async fn stage_cycles(
        &self,
        Parameters(query): Parameters<StageQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let dump_path = query.dump.clone();
        let prefix = query.prefix.clone();
        let clock = query.clock.clone();
        let first = query.from;
        let count = query.count.unwrap_or(80).clamp(1, MAX_CYCLES);
        let named = query.valid.clone();
        let values = query.value.clone();

        self.with_design(query.sources, move |design, _| {
            let mut dump = open_dump(&dump_path)?;
            let flat = rtlscope_analyse::flat::flatten(design);
            let matches = rtlscope_wave::match_signals(&dump, design, &flat, prefix.as_deref());

            // Deepest first, so without a clock the domain reported is the
            // deepest one the dump actually has.
            let report = pipeline::analyse(design);
            let found = match &clock {
                Some(name) => report.domains.iter().find(|domain| {
                    domain.clock == *name || domain.clock.rsplit('.').next() == Some(name.as_str())
                }),
                None => {
                    report.domains.iter().find(|domain| matches.by_ir_name(&domain.clock).is_some())
                }
            };
            let Some(domain) = found else {
                let known: Vec<&str> = report.domains.iter().map(|d| d.clock.as_str()).collect();
                return Err(ErrorData::invalid_params(
                    format!(
                        "no clock domain of this design was found in the dump; the design has \
                         {known:?}. Pass `clock` to name one, or `prefix` if the scope was \
                         inferred wrongly."
                    ),
                    None,
                ));
            };

            let mut layout = rtlscope_wave::Layout::window(first, count);
            layout.valid = stage_pairs(&named)?;
            layout.payload = stage_pairs(&values)?;

            let cycles = rtlscope_wave::stages::cycles(&mut dump, &matches, &domain.clock)
                .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
            let mut view =
                rtlscope_wave::stages::occupancy(&mut dump, &matches, domain, &cycles, &layout);

            // The cells are the answer; the value written in each is a nicety,
            // and a deep pipeline over a long window has too many to be worth
            // carrying.
            if view.rows.len() * view.len() > MAX_CELLS {
                for row in &mut view.rows {
                    row.values.clear();
                }
                view.problems.push(format!(
                    "the value in each cell is left out: {} stage(s) over {} cycle(s) is more \
                     than {MAX_CELLS} of them. Ask for fewer cycles to get them back.",
                    view.rows.len(),
                    view.len()
                ));
            }
            Ok(Json(json(view)))
        })
        .await?
    }

    /// What a run came to, and where in a dump each test sits.
    #[tool(
        name = "test_results",
        description = "\
Read a cocotb `results.xml`: which tests passed, which failed and why, and when \
each one ran. The file records how long each test ran rather than when it \
started, so the moments returned are those durations added up in order - which \
`basis` says, rather than presenting an accumulated number as a recorded one. \
Pass `dump` as well and each moment comes back as a tick of that dump, which is \
where to look for what went wrong."
    )]
    async fn test_results(
        &self,
        Parameters(query): Parameters<ResultsQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        tokio::task::spawn_blocking(move || {
            let run = rtlscope_tb::results::read(std::path::Path::new(&query.results))
                .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;

            let ticks = match &query.dump {
                Some(path) => {
                    let dump = open_dump(path)?;
                    Some(
                        run.tests
                            .iter()
                            .map(|test| dump.ticks_of_ns(test.end_ns))
                            .collect::<Vec<_>>(),
                    )
                }
                None => None,
            };

            Ok(Json(serde_json::json!({
                "tests": run.tests,
                "problems": run.problems,
                "basis": rtlscope_tb::results::BASIS,
                "ticks": ticks,
            })))
        })
        .await
        .map_err(|error| {
            ErrorData::internal_error(format!("reading the results panicked: {error}"), None)
        })?
    }

    /// A second opinion on the IR.
    #[tool(
        name = "yosys_check",
        description = "\
Check RTLScope's reading of a design against a Yosys netlist of the same files: \
which modules exist, what their ports are called and how wide they are, what \
each parameter evaluated to, and what instantiates what. Every other tool here \
derives from the IR, so all of them are wrong the same way if it is; this is \
the only answer that does not come from RTLScope. Produce the netlist with \
`yosys -p \"read_verilog -sv <FILES>; hierarchy -top <TOP>; proc; write_json \
out.json\"` — `proc` is required, since write_json refuses a module that still \
has processes. Read `notes` before trusting a clean result: a check that \
compared nothing also reports no disagreements."
    )]
    async fn yosys_check(
        &self,
        Parameters(query): Parameters<YosysQuery>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let path = query.netlist.clone();
        self.with_design(query.sources, move |design, _| {
            let path = std::path::Path::new(&path);
            if !path.exists() {
                return Err(ErrorData::invalid_params(
                    format!("no netlist at `{}`; paths must be absolute", path.display()),
                    None,
                ));
            }
            let netlist = rtlscope_yosys::read(path)
                .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
            Ok(Json(json(rtlscope_yosys::check(design, &netlist))))
        })
        .await?
    }

    /// What the front end could not read.
    #[tool(
        name = "diagnostics",
        description = "\
List what RTLScope could not read: syntax it does not model, types it could not \
size, ports left unconnected, modules with no source. Everything skipped is \
reported — the front end never models a construct it did not understand — so \
this is the measure of how complete the rest of the answers are."
    )]
    async fn diagnostics(
        &self,
        Parameters(sources): Parameters<Sources>,
    ) -> Result<Json<DiagnosticList>, ErrorData> {
        self.with_design(sources, |design, diags| {
            let errors =
                diags.iter().filter(|d| d.severity == rtlscope_ir::Severity::Error).count();
            let warnings =
                diags.iter().filter(|d| d.severity == rtlscope_ir::Severity::Warning).count();

            let mut ordered: Vec<_> = diags.iter().collect();
            ordered.sort_by_key(|d| d.severity);
            let omitted = ordered.len().saturating_sub(MAX_DIAGNOSTICS);
            let reported = ordered
                .into_iter()
                .take(MAX_DIAGNOSTICS)
                .map(|d| {
                    format!(
                        "{:?}[{}]: {} — {}",
                        d.severity,
                        d.code.id(),
                        d.message,
                        d.span.map_or_else(
                            || "<no location>".to_string(),
                            |span| design.files.render(span)
                        )
                    )
                })
                .collect();

            DiagnosticList { errors, warnings, reported, omitted }
        })
        .await
        .map(Json)
    }
}

impl Server {
    /// Reads the design once, and hands it to whatever wants to ask something.
    ///
    /// Parsing is heavy and blocking, so it happens off the async runtime; the
    /// result is cached against the files and their modification times, since a
    /// conversation asks several questions of the same sources in a row.
    /// Where the design came from, for an answer that wants to say so.
    fn whose(sources: &Sources) -> Option<String> {
        if !sources.files.is_empty() {
            return None;
        }
        Some(rtlscope_sv::Session::read()?.describe())
    }

    async fn with_design<T, F>(&self, sources: Sources, ask: F) -> Result<T, ErrorData>
    where
        F: FnOnce(&Design, &Diagnostics) -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sources, _) = sources.resolved();
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = cache.lock().map_err(|_| {
                ErrorData::internal_error("the design cache was left in a broken state", None)
            })?;

            let key = sources.key();
            if guard.as_ref().is_none_or(|cached| cached.key != key) {
                let (design, diags) = sources.elaborate()?;
                *guard = Some(Cached { key, design, diags });
            }
            let cached = guard.as_ref().expect("just filled");
            Ok(ask(&cached.design, &cached.diags))
        })
        .await
        .map_err(|err| {
            ErrorData::internal_error(format!("reading the design panicked: {err}"), None)
        })?
    }
}

impl Sources {
    /// This query, with the design open in RTLScope filled in when it named no
    /// files of its own.
    ///
    /// Nothing here guesses: an empty `files` is a request for whatever the
    /// window has open, and `whose` is how the answer says which design that
    /// was. Being asked about "the design" and answering about a different one
    /// without saying so is the failure this exists to prevent.
    fn resolved(self) -> (Self, Option<String>) {
        if !self.files.is_empty() {
            return (self, None);
        }
        let Some(session) = rtlscope_sv::Session::read() else { return (self, None) };
        let whose = session.describe();
        (
            Sources {
                files: session.files.iter().map(|path| path.display().to_string()).collect(),
                top: self.top.or(session.top),
                defines: match self.defines.is_empty() {
                    true => session.defines,
                    false => self.defines,
                },
                includes: match self.includes.is_empty() {
                    true => session.includes.iter().map(|p| p.display().to_string()).collect(),
                    false => self.includes,
                },
            },
            Some(whose),
        )
    }

    fn paths(&self) -> Vec<PathBuf> {
        self.files.iter().map(PathBuf::from).collect()
    }

    fn key(&self) -> CacheKey {
        CacheKey {
            files: rtlscope_veryl::watched(&self.paths())
                .into_iter()
                .map(|path| {
                    let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                    (path, stamp)
                })
                .collect(),
            top: self.top.clone(),
            defines: self.defines.clone(),
            includes: self.includes.clone(),
        }
    }

    fn elaborate(&self) -> Result<(Design, Diagnostics), ErrorData> {
        let paths = self.paths();
        if paths.is_empty() {
            return Err(ErrorData::invalid_params(
                "no files given, and no RTLScope window has a design open. Pass the paths \
                 to the SystemVerilog sources to read, or open the design in rtlscope-gui \
                 and ask again without them.",
                None,
            ));
        }
        if let Some(missing) = paths.iter().find(|path| !path.exists()) {
            return Err(ErrorData::invalid_params(
                format!("no file at `{}`; paths must be absolute", missing.display()),
                None,
            ));
        }

        let options = ParseOptions {
            defines: rtlscope_read::parse_defines(&self.defines),
            include_paths: self.includes.iter().map(PathBuf::from).collect(),
        };

        // `Keep`: a `top` given to a tool was named in the question being
        // asked, so being told it is not there is the answer. A window's pin
        // is different — it outlives the read it was chosen in.
        let read = rtlscope_read::read(
            &paths,
            &options,
            self.top.as_deref(),
            rtlscope_read::StaleTop::Keep,
        );
        let diags = read.diags;

        // No design at all means no top module, or a cycle in the hierarchy.
        // The reason is in the diagnostics, so it goes back rather than an
        // empty answer that reads like a design with nothing in it.
        read.design.map(|design| (design, diags.clone())).ok_or_else(|| {
            let reasons: Vec<String> = diags
                .iter()
                .filter(|d| d.severity == rtlscope_ir::Severity::Error)
                .map(|d| d.message.clone())
                .take(5)
                .collect();
            ErrorData::invalid_params(
                format!("the design could not be built: {}", reasons.join("; ")),
                None,
            )
        })
    }
}

/// A trace in words, with the locations rendered.
///
/// `Trace` carries spans, which mean nothing to a reader on the other side of a
/// pipe; this is the same answer with every one of them turned into
/// `file:line`.
fn described(design: &Design, traced: &rtlscope_analyse::Trace) -> serde_json::Value {
    use rtlscope_analyse::drive::Kind;

    let drivers: Vec<serde_json::Value> = traced
        .drivers
        .iter()
        .map(|driver| {
            let (kind, detail) = match &driver.kind {
                Kind::Register { clock, reset } => {
                    ("register", serde_json::json!({ "clock": clock, "reset": reset }))
                }
                Kind::Combinational => ("combinational", serde_json::Value::Null),
                Kind::Latch => ("latch", serde_json::Value::Null),
                Kind::Initial => ("initial", serde_json::Value::Null),
                Kind::FromOutside { port } => ("from outside", serde_json::json!({ "port": port })),
                Kind::FromInstance { instance, of, port } => (
                    "from an instance",
                    serde_json::json!({ "instance": instance, "of": of, "port": port }),
                ),
                Kind::Nothing => ("undriven", serde_json::Value::Null),
            };
            serde_json::json!({
                "kind": kind,
                "detail": detail,
                "at": design.files.render(driver.span),
                "path": driver.path,
                "depends": driver
                    .depends
                    .iter()
                    .map(|on| serde_json::json!({
                        "signal": on.name,
                        // What it is doing there: on the right-hand side, or in
                        // a condition — and if a condition, which one.
                        "through": on.through,
                    }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    serde_json::json!({
        "signal": traced.name,
        "width": traced.width,
        "drivers": drivers,
        "problems": traced.problems,
    })
}

/// Analysis reports are plain `serde` data. Deriving `JsonSchema` for them
/// would put `schemars` into the analysis crate for the sake of one caller, and
/// what each field means is in the tool's own description.
fn json<T: Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// `stage=signal` bindings, where the stage is a number and the signal a name.
fn stage_pairs(given: &[String]) -> Result<Vec<(usize, String)>, ErrorData> {
    given
        .iter()
        .map(|entry| {
            let Some((stage, signal)) = entry.split_once('=') else {
                return Err(ErrorData::invalid_params(
                    format!("`{entry}` is not a binding; write it as `stage=signal`"),
                    None,
                ));
            };
            let stage: usize = stage.trim().parse().map_err(|_| {
                ErrorData::invalid_params(
                    format!("`{entry}` names no stage; the left side is a stage number"),
                    None,
                )
            })?;
            Ok((stage, signal.to_string()))
        })
        .collect()
}

/// Opens a dump, turning a missing file into something a caller can act on.
fn open_dump(path: &str) -> Result<rtlscope_wave::Dump, ErrorData> {
    let path = std::path::Path::new(path);
    if !path.exists() {
        return Err(ErrorData::invalid_params(
            format!("no file at `{}`; paths must be absolute", path.display()),
            None,
        ));
    }
    rtlscope_wave::Dump::open(path)
        .map_err(|error| ErrorData::invalid_params(error.to_string(), None))
}

fn walk(
    design: &Design,
    module_id: rtlscope_ir::ModuleId,
    prefix: &str,
    out: &mut Vec<String>,
    depth: usize,
) {
    // The same guard the analyses use: elaboration has already rejected real
    // recursion, and this is only here so a bug cannot turn into a hang.
    if depth > 64 {
        return;
    }
    for instance in &design.modules[module_id].insts {
        let path = if prefix.is_empty() {
            instance.name.clone()
        } else {
            format!("{prefix}.{}", instance.name)
        };
        out.push(format!("{path} : {}", design.modules[instance.of].name));
        walk(design, instance.of, &path, out, depth + 1);
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        let mut server_info = Implementation::from_build_env();
        server_info.name = "rtlscope".into();
        server_info.title = Some("RTLScope".into());
        // `from_build_env` reports rmcp's own version, not this crate's.
        server_info.version = env!("CARGO_PKG_VERSION").into();

        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info.instructions = Some(
            "Reads synthesisable SystemVerilog, or Veryl, and answers questions about it: what \
                 modules are in a design, how they connect, what its state machines do, \
                 what crosses between its clock domains, where it infers a latch, and how \
                 many clocks deep its logic is.\n\n\
                 Every tool takes the same `files` list, so call `modules` first to see \
                 what is there and to learn the module names the other tools take.\n\n\
                 `files` may be left out entirely. Then the design read is the one open in \
                 RTLScope's window, and `modules` reports in its `source` field which design \
                 that was and how long ago the window said so. This is how to answer \
                 questions about \"the design\" when someone is looking at it: ask without \
                 `files` and say back which one you read.\n\n\
                 It also reads waveform dumps and decodes the protocols on them. Call \
                 `dump_signals` to see what a dump holds, `buses` to have a module's \
                 bindings worked out from its signal names, then `decode` with those \
                 bindings. `stage_cycles` lays a pipeline against a dump's cycles, and \
                 `test_results` says which test failed and at which moment of it.\n\n\
                 Anything RTLScope cannot model is reported rather than guessed at. Call \
                 `diagnostics` to see what was skipped before relying on an answer being \
                 complete, and read a decode's `problems` for the same reason. Every answer \
                 here derives from one reading of the source; `yosys_check` is the only one \
                 that does not, and is what to reach for when an answer looks wrong."
                .into(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = Server::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
