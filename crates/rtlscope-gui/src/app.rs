//! The application: a toolbar, a status bar, and a desk between them.
//!
//! Every view is a tab the reader can put anywhere:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │ ⌁ RTLScope   top / u_rx / u_align   view  clocks fit ◐ │  toolbar
//! ├─────────────┬────────────────────────────────────────┤
//! │ Hierarchy   │ Diagram                                │
//! │             │                                        │
//! ├─────────────┴───────────────────────┬────────────────┤
//! │ Diagnostics │ FSM │ CDC │ Lint │ …  │ Source         │
//! ├─────────────────────────────────────┴────────────────┤
//! │ selected · file:line · editor      3E 135W · status  │  status bar
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! That is where they start, not where they stay: a tab dragged onto another
//! group joins it, dropped on an edge splits it, pulled out of the dock becomes
//! a window floating over the rest. The toolbar and the status bar are the only
//! fixed places left, because they are about the window rather than about the
//! design. Where each view is, and how wide, is the reader's and is remembered
//! between runs — see [`crate::dock`] and [`crate::layout`].
//!
//! The report views are the same ones the CLI prints — the analyses are
//! computed once, lazily, from the same crates — so the window can never say
//! something the terminal would not.
//!
//! **Nothing open is a state, not a failure.** This is a tool someone opens in
//! order to look at a design, which means it has to open before there is one:
//! a window that exits because it was double-clicked without arguments has
//! told the user nothing at all. So [`Open`] holds everything that needs a
//! design, the application holds an `Option` of it, and the empty window is a
//! drop target that says what it takes.
//!
//! Two pieces of state are worth calling out. Layouts are memoised per module,
//! because an immediate-mode GUI runs this code every frame and laying out a
//! diagram sixty times a second would be absurd. And the scene rectangle is
//! stored *per module*, so stepping into a child and back leaves the parent's
//! view exactly where it was rather than resetting it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use egui::{Align, CentralPanel, Layout, Panel, Rect, RichText, ScrollArea, ThemePreference, Ui};
use egui_dock::{DockArea, DockState, TabViewer};
use rtlscope_analyse::flat::SignalId;
use rtlscope_analyse::pipeline::PipelineReport;
use rtlscope_analyse::{DomainReport, Fsm, LintReport};
use rtlscope_graph::block::BlockNode;
use rtlscope_graph::geom::DiagramGeom;
use rtlscope_ir::{Design, Diagnostics, FileTable, ModuleId, NetId, Severity, Span};
use rtlscope_sv::ParseOptions;
use std::fmt::Write as _;

use crate::dock::{self, DockRequest, Tab};
use crate::session::Bookmark;
use crate::theme::{self, Theme};
use crate::views::{self, PipeMode, TraceStep, ViewAction};
use crate::{canvas, tree};

/// The four reports, computed once on first sight of any analysis tab.
///
/// Lazy rather than at startup: parsing already makes the window slow to
/// appear, and someone who opened it to look at the diagram should not also
/// pay for analyses they never look at.
struct Analyses {
    cdc: DomainReport,
    lint: LintReport,
    pipeline: PipelineReport,
}

struct Selected {
    node: BlockNode,
    label: String,
    span: Span,
}

/// A design, and everything that only means anything once there is one.
struct Open {
    design: Design,
    diags: Diagnostics,

    /// The design flattened once, for turning a clicked net into the name a
    /// dump knows it by.
    flat: rtlscope_analyse::flat::Flattened,

    /// Laid out once per module and reused, not recomputed each frame.
    geoms: HashMap<ModuleId, DiagramGeom>,
    /// Kept per module so drilling in and back restores the parent's view.
    scene_rects: HashMap<ModuleId, Rect>,

    /// The drill-down stack; the last entry is what is drawn.
    ///
    /// Each step carries the instance it was entered through, because a dump
    /// knows a signal by its instance path and not by its module.
    path: Vec<tree::Crumb>,
    selected: Option<Selected>,
    highlighted: HashSet<NetId>,

    analyses: Option<Analyses>,
    /// The signal graph the cone is walked over.
    ///
    /// Its own cache rather than one of the four in `Analyses`: those are what
    /// the report tabs need, and a reader tracing one wire should not pay for
    /// a lint pass to find out what feeds it.
    cone_graph: Option<rtlscope_analyse::depth::SignalGraph>,
    /// The state machines, for the same reason and a second one.
    ///
    /// The FSM tab wants them, but so does the waveform, which asks a machine
    /// what its register's values are called before it can spell `S_RUN` on a
    /// row — and that happens the moment a recording is opened, which is not a
    /// moment to run a lint pass. So they live here and `Analyses` reads them
    /// rather than holding a second copy that could disagree.
    fsms: Option<Vec<Fsm>>,
    fsm: views::FsmPane,
    pipeline_selected: usize,
    pipeline_mode: PipeMode,
    /// The two signals the depth strip is asking about, and the answer.
    depth: views::DepthPane,

    /// The line the Source tab is looking at. `None` means the module being
    /// drawn, which is the answer to "where am I" when nothing else was asked.
    source_target: Option<Span>,
    /// The net a run of clicks is walking, and how far along it is.
    walking: Option<Walk>,
    /// The files read so far. Per design, so two views pointing into one file
    /// do not read it twice.
    sources: crate::source::Sources,
    /// How many more frames should ask to be scrolled to the target. More
    /// than one because the first frame does not know how tall the file is.
    source_scroll: u8,
    /// Whether `source_target` is a span into the testbench's files rather
    /// than the design's.
    ///
    /// The two have separate file tables, and a `FileId` means nothing
    /// without its table — so the same span points at two different files
    /// depending on this. Set by the one gesture that looks at the testbench,
    /// the row in the hierarchy; cleared by every gesture that looks at the
    /// design, which all come through [`Open::show_source`].
    source_in_bench: bool,

    /// The files this design was read from, as the reader named them, so it
    /// can be read again without asking for them a second time.
    source_paths: Vec<PathBuf>,
    /// The SystemVerilog actually read: the same files for a design written
    /// in it, and what Veryl wrote for one written in Veryl. What a simulator
    /// is handed, since it reads no Veryl either.
    sources_read: Vec<PathBuf>,
    /// The files whose change should have the design read again. The sources
    /// themselves for SystemVerilog; for Veryl the `.veryl` files and the
    /// manifest, and not the output a change will cause to be rewritten.
    watch_paths: Vec<PathBuf>,

    /// The provenance walk: each hop a net and the place it lives, oldest
    /// first. Empty until a wire is clicked.
    trail: Vec<views::Hop>,
    /// The cone, when the Trace tab is showing one instead of the text.
    ///
    /// Kept beside the trail rather than inside it: a hop is a step in a hunt,
    /// and which way the reader was looking at it is not part of the step.
    cone: ConePane,

    /// Why the newest read did not come out as a design.
    ///
    /// Present means everything else here is the previous version. The file
    /// table comes with it because these diagnostics point into the sources as
    /// they are now, and rendering them against the design's own table would
    /// name the wrong files.
    stale: Option<Stale>,

    /// Where every register sits in its pipeline, worked out the first time a
    /// diagram asks and kept for the rest.
    ///
    /// Kept here rather than computed per box because it comes from
    /// flattening the whole design, and a reader drilling through forty
    /// modules should not pay for that forty times. Dropped with the design:
    /// a re-read is a new design, and the stages of the old one are not a fact
    /// about it.
    stages: Option<rtlscope_graph::Stages>,
}

/// Sources that have been changed into something that does not read.
struct Stale {
    files: FileTable,
    diags: Diagnostics,
    /// Whether this came of adding files rather than of editing them. The two
    /// leave the window in the same state and mean opposite things: one says
    /// the files on disk have gone past what is drawn, the other says nothing
    /// happened at all.
    from_add: bool,
}

/// Whether files arriving are the design, or join the design.
///
/// Everything that arrives — a drop, a dialog, a sample — used to mean the
/// first, and for a drop it still does: dropping a project on a window showing
/// another one opens it, rather than welding two unrelated designs together.
/// But a design whose parts live in two places could then only be opened by
/// naming all of them at once, and the window's own refusal to simulate says
/// "add their sources" to somebody with no way to do it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bring {
    /// The design is what these files say, and nothing else.
    #[default]
    Replace,
    /// These files are read together with the ones already open.
    Add,
    /// These files are a testbench, which is neither of the above: it is not
    /// the design and it does not join the design. It is what runs against it.
    Testbench,
}

impl Open {
    fn current(&self) -> ModuleId {
        self.path.last().map_or(self.design.top, |crumb| crumb.module)
    }

    /// The layout for a module, computed on first sight and kept.
    fn geom(&mut self, module: ModuleId) -> &DiagramGeom {
        let stages = self.stages.get_or_insert_with(|| rtlscope_graph::Stages::of(&self.design));
        let design = &self.design;
        self.geoms
            .entry(module)
            .or_insert_with(|| rtlscope_graph::diagram_staged(design, module, stages))
    }

    fn ensure_cone_graph(&mut self) {
        if self.cone_graph.is_none() {
            self.cone_graph = Some(rtlscope_analyse::depth::signal_graph(&self.design, &self.flat));
        }
    }

    fn ensure_fsms(&mut self) {
        if self.fsms.is_none() {
            self.fsms = Some(rtlscope_analyse::fsm::find(&self.design));
        }
    }

    /// The machines found so far — none, until somebody has asked.
    ///
    /// Empty rather than absent, because every caller wants to look through
    /// them and a design with no machines and a design nobody has looked at
    /// come to the same thing for that.
    fn state_machines(&self) -> &[Fsm] {
        self.fsms.as_deref().unwrap_or_default()
    }

    fn ensure_analyses(&mut self) {
        if self.analyses.is_none() {
            self.analyses = Some(Analyses {
                cdc: rtlscope_analyse::cdc::analyse(&self.design),
                lint: rtlscope_analyse::lint::analyse(&self.design),
                pipeline: rtlscope_analyse::pipeline::analyse(&self.design),
            });
        }
    }

    fn goto(&mut self, module: ModuleId) {
        self.path = tree::path_to(&self.design, module);
        self.selected = None;
        self.highlighted.clear();
    }

    /// Where the Source tab should be looking: what was asked for, or — when
    /// nothing was — the module being drawn.
    fn source_span(&self) -> Span {
        self.source_target.unwrap_or_else(|| self.design.module(self.current()).span)
    }

    fn show_source(&mut self, span: Span) {
        self.source_target = Some(span);
        self.source_scroll = SCROLL_FRAMES;
        self.source_in_bench = false;
    }

    /// Points the Source tab at the testbench's top module.
    ///
    /// The other way in: the span is into the bench's own table, so the flag
    /// goes up with it, and stays up until something points at the design.
    fn show_bench_source(&mut self, span: Span) {
        self.source_target = Some(span);
        self.source_scroll = SCROLL_FRAMES;
        self.source_in_bench = true;
    }

    /// Where the reader is, in names that will still mean something once the
    /// sources have been read again. See [`crate::session`].
    fn bookmark(&self) -> Bookmark {
        let module = self.design.module(self.current());
        Bookmark {
            top: Some(self.design.module(self.design.top).name.clone()),
            instances: self.path.iter().filter_map(|crumb| crumb.instance.clone()).collect(),
            selected: self.selected.as_ref().map(|selected| selected.label.clone()),
            highlighted: self
                .highlighted
                .iter()
                .filter_map(|net| module.nets.get(*net).map(|net| net.name.clone()))
                .collect(),
            camera: self
                .scene_rects
                .iter()
                .map(|(module, rect)| (self.design.module(*module).name.clone(), *rect))
                .collect(),
            fsm: self
                .state_machines()
                .get(self.fsm.selected)
                .map(|machine| format!("{}.{}", machine.module_name, machine.state_name)),
            pipeline_selected: self.pipeline_selected,
            source_at: self.source_target.and_then(|span| {
                let path = self.design.files.path(span.file)?.to_path_buf();
                Some((path, span.line, span.col, span.len))
            }),
            trail: self
                .trail
                .iter()
                .filter_map(|hop| {
                    let net = self.design.module(hop.module()).nets.get(hop.net)?;
                    let instances =
                        hop.path.iter().filter_map(|crumb| crumb.instance.clone()).collect();
                    Some((instances, net.name.clone()))
                })
                .collect(),
        }
    }
}

/// How often the sources are looked at.
///
/// Polled rather than watched by the system: the list is the files this design
/// was read from — tens of them at most — and one `metadata` call each is
/// cheaper than a filesystem-watching dependency and the threads it brings.
const WATCH_EVERY: Duration = Duration::from_millis(400);

/// How often the window says again which design it has open, so that the age
/// of the note tells a reader whether a window is still there.
const SESSION_REFRESH: Duration = Duration::from_secs(120);

/// How long the sources have to hold still before they are read.
///
/// An editor saving a file can touch it more than once, and half a file reads
/// as nonsense. Waiting for two looks to agree makes a save that writes several
/// files one read rather than several.
const SETTLE: Duration = Duration::from_millis(300);

/// Looks at the sources on a thread of its own, and wakes the window when they
/// change.
///
/// It has to be a thread. An immediate-mode window with nothing happening does
/// not repaint — that is what makes it cheap to leave open — so a poll on the
/// UI thread would either never run or have to wake sixty times a second to
/// run. A thread that does the looking and wakes the window only when there is
/// something to see costs a `stat` per file per 400ms and nothing else.
struct SourceWatch {
    /// What it is watching, so a design read from other files gets a new one.
    paths: Vec<PathBuf>,
    changed: Receiver<()>,
}

/// Watches until the window stops listening.
///
/// Dropping the receiver — which is what replacing the design does — makes the
/// next send fail, and that is how this thread learns to stop.
fn watch_files(paths: Vec<PathBuf>, ctx: egui::Context, changed: Sender<()>) {
    let mut stamps = stamps_of(&paths);
    loop {
        std::thread::sleep(WATCH_EVERY);
        let seen = stamps_of(&paths);
        if seen == stamps {
            continue;
        }

        // Let the writing finish before saying anything: two looks that agree.
        let mut settled = seen;
        loop {
            std::thread::sleep(SETTLE);
            let again = stamps_of(&paths);
            if again == settled {
                break;
            }
            settled = again;
        }
        stamps = settled;

        if changed.send(()).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// What a background read of the sources came to.
struct Reread {
    files: FileTable,
    diags: Diagnostics,
    design: Option<Design>,
    /// See [`Open::sources_read`] and [`Open::watch_paths`]: a re-read of a
    /// Veryl project can add a file, and the list to watch moves with it.
    sources_read: Vec<PathBuf>,
    watch_paths: Vec<PathBuf>,
}

/// When each source was last written, as of now.
///
/// A file that cannot be read is recorded as `None` rather than left out, so a
/// file appearing or disappearing is itself a change.
fn stamps_of(paths: &[PathBuf]) -> Vec<(PathBuf, Option<SystemTime>)> {
    paths
        .iter()
        .map(|path| (path.clone(), std::fs::metadata(path).and_then(|at| at.modified()).ok()))
        .collect()
}

/// How many frames a request to scroll to a line is repeated over.
const SCROLL_FRAMES: u8 = 3;

/// How many columns a new drawing starts with.
///
/// Enough for a pipeline to fill and drain a few times, few enough to fit on a
/// screen without scrolling. The reader adds more when they need them.
const STIM_COLUMNS: u64 = 32;

/// How many cycles a simulation started from the window runs for, until the
/// reader says otherwise.
///
/// Enough cycles that a pipeline fills and drains several times, few enough
/// that the wait is seconds rather than minutes.
const SIM_CYCLES: u64 = 2_000;

/// The narrowest and widest a run may be asked to be.
///
/// The floor is a pipeline's worth: fewer than this and a design has not
/// finished resetting, so the waveform says nothing about it. The ceiling is
/// where a dump stops being something to look at and becomes something to
/// store — a million cycles of a wide design is gigabytes, and the panel draws
/// what is on screen, not what would have to be read to get there.
const SIM_CYCLES_RANGE: std::ops::RangeInclusive<u64> = 16..=200_000;

/// What a background simulation has to say.
enum SimMessage {
    #[allow(dead_code)]
    Progress(String),
    /// Boxed because the error side carries a simulator's whole output, and an
    /// enum is as large as its largest variant.
    Done(Box<Result<Ran, String>>),
}

/// A dump, and — when a pattern was played — what it came to.
struct Ran {
    dump: PathBuf,
    verdict: Option<rtlscope_tb::Verdict>,
    /// Which simulator actually produced it, and how long it took.
    ///
    /// Said out loud because the two are not alike. Verilator compiles the
    /// design to a program; Icarus interprets it, and is slower by a factor
    /// that turns a few seconds into a minute. When the faster one is refused
    /// the fallback is silent, and a reader is left thinking their design is
    /// slow rather than their toolchain broken — measured on a machine whose
    /// `g++` fails to compile `int main() {}`, where every run had quietly been
    /// interpreted.
    how: String,
}

/// Runs a testbench the reader wrote, against the design's own sources.
///
/// No elaboration and no generation: the design has already been read, and
/// what is under test here is their file rather than anything this program
/// would write. The sources still have to be absolute — the simulator runs
/// from its own directory.
fn run_written_bench(
    paths: &[PathBuf],
    bench: &rtlscope_tb::Bench,
    engine: rtlscope_tb::Engine,
    tools: &rtlscope_tb::Tools,
) -> Result<Ran, String> {
    let absolute: Vec<PathBuf> = paths
        .iter()
        .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect();
    let tools = tools.clone().near_sources(&absolute);
    let work = std::env::temp_dir().join("rtlscope-sim").join(&bench.top);
    let started = std::time::Instant::now();
    let outcome = rtlscope_tb::bench::simulate(engine, &work, bench, &absolute, &tools)
        .map_err(|error| error.to_string())?;
    Ok(Ran {
        dump: outcome.dump,
        verdict: None,
        how: format!(
            "`{}` on {} in {:.1}s",
            bench.top,
            outcome.engine.name(),
            started.elapsed().as_secs_f32()
        ),
    })
}

/// Reads the sources again, writes a harness and runs it.
///
/// Off the UI thread, so nothing here may touch the application. The stimulus
/// decides almost everything else: random goes straight to a simulator and
/// makes a waveform, a drawing goes through cocotb and makes a verdict.
/// The simulators the window can be told to use, in the order Settings lists
/// them: the default first.
/// What opens a source location when nobody has said otherwise.
const DEFAULT_EDITOR: &str = "code -g {file}:{line}:{col}";

const ENGINES: [rtlscope_tb::Engine; 2] =
    [rtlscope_tb::Engine::Verilator, rtlscope_tb::Engine::Icarus];

/// What each tool answers, in the one line a launcher has room for.
///
/// Written here rather than taken from the CLI's help text, which is the same
/// sentence in a place this crate cannot reach without depending on it.
fn what_it_answers(tab: Tab) -> &'static str {
    match tab {
        Tab::Diagnostics => "What could not be read, and the line it was on.",
        Tab::Fsm => "The state machines: their states, transitions and guards.",
        Tab::Cdc => "The clock domains, and every signal that crosses between them.",
        Tab::Lint => "Inferred latches, signals nobody reads, modules nobody instantiates.",
        Tab::Pipeline => "How many clocks deep the logic is, and what sits at each depth.",
        Tab::Stim => "Draw a waveform, run it, and be told whether it held.",
        Tab::Trace => "Where a signal came from, hop by hop.",
        Tab::Wave => "A recording, read against the design that made it.",
        // Not listed: the launcher does not launch itself, and the three views
        // that show the design rather than report on it are the window.
        _ => "",
    }
}

/// The keys the window itself listens for, and what each one does.
///
/// The three tables sit out here rather than inside the menu that draws them,
/// so that everything the help claims can be read in one place and checked
/// against the code that has to be true for it — and so the call that draws a
/// section is one line rather than a page.
const HELP_KEYS: [(&str, &str); 5] = [
    ("Ctrl+O", "Open a design."),
    ("Ctrl+P", "Find a module or a signal by name, once one is open."),
    ("↑ ↓", "Move through what the search found."),
    ("Enter", "Go to it."),
    ("Esc", "Put the search away."),
];

/// The keys the waveform listens for, which it only does while the pointer is
/// over it.
const HELP_WAVE: [(&str, &str); 5] = [
    (
        "← →",
        "To the selected signal's previous or next edge. Click a signal's name first — an \
         edge belongs to one signal.",
    ),
    (
        "alt + ↑ ↓",
        "Move the rows you picked up or down. Dragging a name does the same, and is the \
         faster way when the row has far to go.",
    ),
    ("M", "Put a marker where the cursor is, or take it down."),
    ("drag", "Sweep a band to zoom into it. Esc abandons the sweep."),
    ("right-click", "Ask about the moment under the pointer."),
];

/// What the pointer does, by the view that answers it. Grouped that way because
/// a reader arrives at this list already looking at one view and wanting to
/// know what it will do, not holding a gesture and wondering where it works.
const HELP_GESTURES: [(&str, &str); 5] = [
    ("Diagram", "Double-click a block to go inside it. The way back is the path in the toolbar."),
    (
        "Wave",
        "Drag a name to move that row, and a module's line to move everything under it. A \
         line shows where they will land; Esc puts them back. Shift-drag pans the view, as \
         it does over the waveform.",
    ),
    (
        "FSM",
        "Double-click a state to open the source it was written in. With a recording open, \
         the state the machine is in at the cursor wears a ring in the cursor's own colour, \
         and `next ▸` moves the cursor to when the state you clicked is next entered.",
    ),
    ("Trace", "Double-click a signal to ask where that one came from."),
    (
        "Stim",
        "On a one-bit lane, click a cell to cycle it or drag to paint a run of them. On a bus, \
         click to type a value and right-click to put it back to not-drawn.",
    ),
];

/// Which instance's state register a machine's diagram is about.
///
/// A module instantiated twice holds two registers, and a dump knows them apart
/// by path. The one the reader has drilled into wins; failing that the first,
/// because a machine looked at from outside any instance is still a machine and
/// the first is a better answer than none — the view says which was used, so a
/// reader in the other one can see that it is the other one.
///
/// Returns the flattened signal and the path it was found under.
fn state_signal_of(
    flat: &rtlscope_analyse::flat::Flattened,
    fsm: &Fsm,
    crumbs: &[tree::Crumb],
) -> Option<(SignalId, String)> {
    let here = tree::instance_path(crumbs);
    let mut first = None;
    for node in flat.nodes.iter().filter(|node| node.module == fsm.module) {
        let Some(signal) = node.signal(fsm.state) else { continue };
        if node.path == here {
            return Some((signal, node.path.clone()));
        }
        first.get_or_insert((signal, node.path.clone()));
    }
    first
}

/// What the recording says the machine is doing, at the cursor.
///
/// A free function because this is the one place a waveform and a state machine
/// are put to one another, and neither owns the other: the panel knows values
/// and times, the machine knows what a value means, and the answer is no use to
/// either of them alone.
fn now_for(
    fsm: &Fsm,
    flat: &rtlscope_analyse::flat::Flattened,
    crumbs: &[tree::Crumb],
    wave: Option<&mut crate::wave::WaveState>,
) -> Option<views::Now> {
    let wave = wave?;
    let at = wave.cursor?;
    let (signal, path) = state_signal_of(flat, fsm, crumbs)?;
    let value = wave.value_at_cursor(signal).and_then(|held| held.as_u64());
    let state = value.and_then(|held| {
        let held = i64::try_from(held).ok()?;
        fsm.states.iter().position(|state| state.value == held)
    });
    Some(views::Now { at, state, value, instance: (!path.is_empty()).then_some(path) })
}

/// The rows of one help section: what to do on the left, what it does on the
/// right.
///
/// A grid rather than a row of labels, so the second column starts in the same
/// place on every line. A list whose meanings begin wherever the gesture
/// happened to end has to be read; one that lines up can be scanned, and
/// scanning is the whole of what somebody who opened this menu came for.
///
/// `mono` because the two kinds of left-hand column are not the same kind of
/// thing: a key is typed exactly as it is written and reads as type, a view is
/// a name and reads as one.
fn help_rows(ui: &mut Ui, id: &str, mono: bool, rows: &[(&str, &str)]) {
    egui::Grid::new(id).num_columns(2).spacing([14.0, 4.0]).show(ui, |ui| {
        for (what, does) in rows {
            let left = RichText::new(*what).small();
            ui.label(match mono {
                true => left.monospace(),
                false => left.strong(),
            });
            ui.add(egui::Label::new(RichText::new(*does).small().weak()).wrap());
            ui.end_row();
        }
    });
}

/// What each simulator is, in the one line the choice needs.
/// Splits a command line the way a shell would: on spaces, except inside
/// quotes.
///
/// An editor lives somewhere like `C:\Program Files\...`, so the program
/// itself has to be quotable. Splitting on whitespace alone made that command
/// unusable and there was no other way to say it.
fn split_command(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in line.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// A path out of a box somebody types in, or nothing at all.
///
/// Trimmed of quotes as well as space: "Copy as path" in Explorer wraps what it
/// gives you in `"`, and pasting that is the ordinary way to fill one of these
/// boxes. Rejecting it as a path that does not exist would be blaming the
/// reader for using the button Windows gave them.
fn filled_in(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim().trim_matches('"').trim();
    match trimmed.is_empty() {
        true => None,
        false => Some(PathBuf::from(trimmed)),
    }
}

fn what_it_is(engine: rtlscope_tb::Engine) -> &'static str {
    match engine {
        rtlscope_tb::Engine::Verilator => {
            "Compiles the design to C++. Fast once built, and needs a C++ compiler."
        }
        rtlscope_tb::Engine::Icarus => {
            "Interprets the design. Needs nothing else, and refuses some constructs."
        }
    }
}

// One more than clippy likes, and the extra one is `tools`: the alternative is
// a struct that exists only to carry seven things across a `spawn`.
#[allow(clippy::too_many_arguments)]
fn simulate_off_thread(
    paths: &[PathBuf],
    options: &ParseOptions,
    top: Option<&str>,
    module: &str,
    stimulus: rtlscope_tb::Stimulus,
    cycles: u64,
    engine: rtlscope_tb::Engine,
    tools: rtlscope_tb::Tools,
) -> Result<Ran, String> {
    let (uir, _) = rtlscope_sv::lower_files(paths, options);
    let Some(design) = rtlscope_elab::elaborate(&uir, top).0 else {
        return Err("the sources no longer elaborate into a design".to_string());
    };
    let Some((module_id, _)) = design.module_by_name(module) else {
        return Err(format!("`{module}` is no longer in the design"));
    };
    if let Some(refusal) = unbuildable(&design, module_id, module) {
        return Err(refusal);
    }

    let absolute: Vec<PathBuf> = needed(&uir, &design, paths)
        .iter()
        .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect();
    // Anchored at the design's own files. A window is handed those and nothing
    // else it can trust: its working directory is Explorer's choice, and the
    // directory it was installed into holds no venv. A checkout that has one
    // keeps it at the root, which is somewhere above the RTL — so looking up
    // from a source file finds it, and looking up from anything else does not.
    let tools = tools.near_sources(&absolute);
    let drawn = matches!(stimulus, rtlscope_tb::Stimulus::Drawn(_));
    let cycles = match &stimulus {
        // A drawing says how long it is; running fewer cycles than it has
        // columns would leave part of it unplayed and say nothing about that.
        rtlscope_tb::Stimulus::Drawn(pattern) => pattern.cycles + 4,
        _ => cycles,
    };
    let tb = rtlscope_tb::TbOptions {
        cycles,
        sources: absolute.iter().map(|path| path.display().to_string()).collect(),
        stimulus,
        ..rtlscope_tb::TbOptions::default()
    };
    // A drawing is played by cocotb, which reads it as data; random stimulus
    // goes straight to a simulator and needs no Python at all.
    let flavor = match drawn {
        true => rtlscope_tb::Flavor::Cocotb,
        false => rtlscope_tb::Flavor::Sv,
    };
    let generated = rtlscope_tb::generate_with(&design, module_id, &tb, flavor)
        .map_err(|error| error.to_string())?;

    // Into the scratch directory, not the user's: a window they opened to look
    // at something should not leave build output in whatever folder it was
    // started from.
    let base = design.modules[module_id].base_name.clone();
    let work = std::env::temp_dir().join("rtlscope-sim").join(&base);

    if drawn {
        let outcome = rtlscope_tb::run::play(&work, &base, &generated.files, &tools, engine)
            .map_err(|error| error.to_string())?;
        let verdict = rtlscope_tb::Verdict::read(&work.join(format!("{base}_verdict.json"))).ok();
        let how = format!("cocotb on {}", engine.name());
        return Ok(Ran { dump: outcome.dump, verdict, how });
    }

    // Whichever the reader chose in Settings, and Verilator until they choose.
    // What there is not, and never was, is a fallback from one to the other:
    // Icarus interprets rather than compiles, which is slower by a factor that
    // turns seconds into a minute, and it refuses constructs Verilator takes —
    // so falling back to it turned a clear "install Verilator" into a long wait
    // or an error about syntax the faster tool had no trouble with. Being asked
    // for by name is a different thing from being reached for, and the setting
    // is the first.
    if !tools.available(engine) {
        return Err(format!(
            "`{}` was not found, and the window simulates with nothing it was not asked \
             for.\n  Install it with `{}`, then name the directory it is in under \
             `settings` — a window started from Explorer inherits no shell's PATH, which \
             is what that box is for.\n  Or pick the other simulator there.",
            engine.name(),
            engine.how_to_get_it()
        ));
    }
    let began = std::time::Instant::now();
    match rtlscope_tb::simulate(engine, &work, &base, &generated.files, &absolute, &tools) {
        Ok(outcome) => {
            let how = format!("{} in {:.0}s", engine.name(), began.elapsed().as_secs_f32());
            Ok(Ran { dump: outcome.dump, verdict: None, how })
        }
        Err(error) => Err(format!("could not simulate `{base}`.\n{error}")),
    }
}

/// Why this design cannot be simulated, when it cannot.
///
/// A module RTLScope has no source for is drawn as a black box and read as one,
/// which is right: the design is still a design and its wiring is still worth
/// looking at. A simulator has no such option. Vendor primitives — a clock
/// manager, an output serialiser — are exactly what a board-level top is made
/// of, so this is the common case and not an edge one, and the compiler error
/// it otherwise produces names a file the reader did not write.
///
/// Said before anything is generated, so the answer arrives in a second rather
/// than after a build.
///
/// Only what *this* module reaches. A design can hold a black box in a corner
/// the chosen module never touches, and refusing then would contradict the
/// advice this very message gives — open something below the primitives and
/// simulate that.
fn unbuildable(design: &Design, from: rtlscope_ir::ModuleId, module: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let mut stack = vec![from];
    let mut missing: Vec<&str> = Vec::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let here = design.module(id);
        if here.is_blackbox {
            missing.push(here.name.as_str());
            continue;
        }
        stack.extend(here.insts.iter().map(|inst| inst.of));
    }
    if missing.is_empty() {
        return None;
    }
    missing.sort_unstable();
    missing.dedup();

    let named = missing.iter().take(4).copied().collect::<Vec<_>>().join(", ");
    let rest = match missing.len() > 4 {
        true => format!(" and {} more", missing.len() - 4),
        false => String::new(),
    };
    Some(format!(
        "`{module}` instantiates {} module(s) RTLScope has no source for ({named}{rest}), and a \
         simulator cannot build what it cannot see. Add their sources with `open` → \
         `add sources…`, or open a module below them and simulate that.",
        missing.len()
    ))
}

/// The files a simulator has to read to build this design.
///
/// Not the files that were opened. A folder dropped on the window brings in
/// everything under it — measured: forty-two files for a design that uses
/// eight — and handing all of them to a compiler asks it to build modules the
/// top never instantiates. That is not merely wasteful: those modules are
/// where the unsupported syntax lives, so the simulation fails on files the
/// reader was not asking about. Icarus refused this design with sixty
/// complaints, none of them from a module in it.
///
/// Elaboration already keeps only what the top reaches, so a module's span
/// names a file that matters. A file holding no module at all is kept anyway:
/// that is where a compilation-unit `typedef` lives, and dropping it would
/// break the files that were kept.
///
/// The given order is preserved, because a compiler reads a file list in order
/// and a declaration has to arrive before its use.
///
/// Both sides are canonicalised before they are compared. The frontend records
/// the path it opened, which is not the spelling it was handed — measured: the
/// window passes `tests\fixtures\rtl\hdmi\hdmi_output.sv` and the file table
/// holds the absolute form, so a plain comparison matches nothing and every
/// file looks like one that declares no module.
fn needed(uir: &rtlscope_ir::UDesign, design: &Design, given: &[PathBuf]) -> Vec<PathBuf> {
    let same = |path: &Path| dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let reached: HashSet<PathBuf> = design
        .modules
        .iter()
        .filter_map(|module| design.files.path(module.span.file))
        .map(same)
        .collect();
    let defines_a_module: HashSet<PathBuf> = uir
        .modules
        .iter()
        .filter_map(|module| uir.files.path(module.span.file))
        .map(same)
        .collect();

    let keep: Vec<PathBuf> = given
        .iter()
        .filter(|path| {
            let path = same(path);
            reached.contains(&path) || !defines_a_module.contains(&path)
        })
        .cloned()
        .collect();

    // Nothing recognised means the spans and the paths are spelled differently
    // — a relative path against an absolute one, say. Handing back everything
    // is then wrong but harmless, where handing back nothing is a simulation
    // with no design in it.
    match keep.is_empty() {
        true => given.to_vec(),
        false => keep,
    }
}

/// How the Trace tab is showing a signal's surroundings.
///
/// The default is the text, which is what the tab has always been and what
/// answers "where exactly did this come from" — a line and a condition. The
/// cone answers the wider question, "what is around this at all", and the two
/// are worth having side by side rather than one replacing the other.
pub struct ConePane {
    /// Whether the drawing is showing rather than the provenance text.
    pub drawn: bool,
    pub towards: rtlscope_analyse::cone::Towards,
    pub depth: usize,
    /// Where the drawing sits and how big it is drawn.
    placement: crate::canvas::Placement,
}

impl Default for ConePane {
    fn default() -> Self {
        Self {
            drawn: false,
            towards: rtlscope_analyse::cone::Towards::Drivers,
            // Three reaches through two registers and the logic between them,
            // which is one pipeline stage either side of the signal — about as
            // much as fits on a screen and is worth taking in at once.
            depth: 3,
            placement: crate::canvas::Placement::default(),
        }
    }
}

/// Sources that were read and did not come out as a design.
///
/// Kept rather than discarded: "could not elaborate" is an answer and the
/// reasons are the useful part of it. Without this the window would fall back
/// to its empty state and look as though the drop had been ignored.
struct Failed {
    files: FileTable,
    diags: Diagnostics,
    what: String,
    /// The modules nothing instantiates, when that is why it failed.
    ///
    /// A project folder holds several designs and the elaborator cannot guess
    /// which one was meant. On the command line the answer is `--top`; a
    /// window that only repeated that advice would be telling somebody to
    /// close it and start again, so the names become buttons.
    tops: Vec<String>,
    /// What was read, so a chosen top can be read again without asking for the
    /// files a second time.
    paths: Vec<PathBuf>,
}

pub struct RtlScopeApp {
    open: Option<Open>,
    failed: Option<Failed>,

    /// How the sources were read on the command line, reused for anything
    /// dropped in later so a design with `ifdef`s behaves the same either way.
    options: ParseOptions,
    top: Option<String>,

    /// A testbench the reader wrote, when they have given one.
    ///
    /// Beside the design rather than part of it, which is the whole point of
    /// reading it separately: a testbench instantiates the thing under test
    /// and is therefore a second module nothing instantiates. Read them
    /// together and "which of these is the design" becomes a coin toss —
    /// measured, in the same shape as the ambiguity an added file causes.
    /// Kept apart, the diagram goes on being of the design while the waveform
    /// is of the run.
    bench: Option<rtlscope_tb::Bench>,
    /// The testbench's files, read for the Source tab. Its own cache rather
    /// than the design's, because that one is keyed by `FileId` and the two
    /// tables number their files from zero independently.
    bench_sources: crate::source::Sources,

    /// The dump being looked at, when one is open.
    ///
    /// On the application rather than on the design, because a recording does
    /// not need one. It was on `Open` for as long as the panel was thought of
    /// as a second layer over the diagram, and that placement was the whole of
    /// why a `.vcd` dropped on its own opened nothing: there was nowhere to put
    /// it. The design is what *names* the rows; the rows themselves are in the
    /// file. Here it also survives the sources being read again, or replaced,
    /// and is rebound against whatever they now say.
    wave: Option<crate::wave::WaveState>,

    /// A simulation running in the background, and the last thing it said.
    ///
    /// On another thread because a Verilator build takes ten seconds or more,
    /// and a window that stops answering for ten seconds is a window that looks
    /// broken.
    sim: Option<Receiver<SimMessage>>,
    sim_progress: String,
    /// Why the last simulation produced nothing, shown where it was asked for.
    sim_refused: Option<String>,
    /// How many cycles the next simulation runs for.
    ///
    /// Settable because the right number is a fact about the design and not
    /// about the tool: a handshake that completes in forty cycles wants a
    /// short run to read, and a frame of video wants a long one to reach the
    /// interesting part at all.
    sim_cycles: u64,
    /// Which simulator the window runs.
    ///
    /// Verilator until the reader says otherwise in Settings, and no automatic
    /// fallback either way: see [`simulate_off_thread`].
    engine: rtlscope_tb::Engine,
    /// Whether each simulator's tools were found.
    ///
    /// Looked up once and kept, because the answer comes off the filesystem and
    /// the settings tab would otherwise scan the whole PATH on every frame it
    /// is left open. Emptied when [`App::sim_dir`] is edited, which is the one
    /// thing that can change the answer without the filesystem changing.
    sim_found: BTreeMap<&'static str, bool>,
    /// Where the simulator is, when the `PATH` does not say.
    ///
    /// The installer makes this window what opens a `.sv` file, and a process
    /// started that way inherits no terminal's `PATH` and stands wherever
    /// Explorer left it. Both of the things a simulation needs — the simulator,
    /// and a `.venv-cocotb` to play a pattern with — were reachable only from a
    /// developer's shell until these two boxes existed.
    ///
    /// Text rather than a `PathBuf`, because that is what the box holds and a
    /// half-typed path is not a path yet. Empty means "look the old way".
    sim_dir: String,
    /// The Python that plays a drawn pattern, for the same reason.
    python: String,
    /// The name box, when it is open.
    ///
    /// On the application rather than on the open design: it is a way of
    /// getting somewhere, and it should not be thrown away and rebuilt every
    /// time the design is read again.
    palette: crate::palette::Palette,

    /// A read of the sources running in the background, and the thread that
    /// notices they need one.
    reload: Option<Receiver<Box<Reread>>>,
    watch: Option<SourceWatch>,
    /// Which view asked for it. Opening a dump moves to the waveform, which is
    /// right when a file was dropped and wrong here: someone who pressed
    /// `simulate` on the flow view asked to see the flow.
    sim_from: Tab,

    /// The files open for editing, by path — which is what survives the
    /// design being read again, as a `FileId` does not.
    /// True while `compare…` is waiting for a recording to be dropped.
    awaiting_reference: bool,
    /// A file dialog open on another thread, and what it will come back with.
    ///
    /// Not on this thread: the dialog pumps a message loop of its own for as
    /// long as it is up, and a window that stopped drawing while one was open
    /// would leave the waveform beside it frozen. An empty list means the
    /// dialog was cancelled.
    picker: Option<Receiver<(Bring, Vec<PathBuf>)>>,

    /// The desk: every view, and where the reader has put it.
    ///
    /// An `Option` because it has to be handed out. `DockArea` borrows the
    /// whole state to draw, and drawing a view can want to move another one —
    /// a location clicked in the FSM brings the source forward — so the dock is
    /// taken out for the length of one frame's drawing and put back after. It
    /// is `None` only in that window, and `show`/`detach` know to write the
    /// move down instead when it is.
    dock: Option<DockState<Tab>>,
    /// Moves asked for while the dock was out, made the moment it is back.
    dock_requests: Vec<DockRequest>,
    /// Which views were on screen when the dock was last handed out.
    ///
    /// Read by anything that runs while the dock is out and needs to know
    /// whether a view can be seen — `PointAt` is the one that matters, since a
    /// view only follows the reader's eye to a line if the source is already
    /// somewhere they are looking. A frame old, and that is fine: it is the
    /// arrangement they are looking at as they click.
    visible: BTreeSet<Tab>,
    /// Where each floating window was last seen, by the number of the surface
    /// it belongs to.
    ///
    /// Kept here rather than in the dock because the dock does not record it —
    /// see `dock::float_places`. Watched every frame, like the main window's
    /// size and for the same reason.
    floats: BTreeMap<usize, egui::Rect>,
    /// How big the main window is, watched so it can be written down.
    ///
    /// Read every frame rather than on the way out, because a process killed by
    /// the session ending never gets to say goodbye.
    main_size: Option<[f32; 2]>,

    /// The drawings, by module name. Kept on the application rather than on the
    /// design so that reading the sources again does not throw away what
    /// somebody drew; `Pattern::reconcile` squares them up afterwards.
    patterns: HashMap<String, rtlscope_tb::Pattern>,
    stim: crate::stim::StimPane,
    /// What the last drawing played came to, and anything its rows could not be
    /// squared with in the design.
    verdict: Option<rtlscope_tb::Verdict>,
    stim_problems: Vec<String>,

    /// The design the session note currently claims, and when it last said so.
    published: Option<Vec<PathBuf>>,
    published_at: Option<Instant>,

    theme_preference: ThemePreference,
    show_clocks: bool,
    /// How to open a file at a line, e.g. `code -g {file}:{line}:{col}`.
    /// How a source location is opened, with `{file}`, `{line}` and `{col}`
    /// substituted.
    editor_command: String,
    /// Whether `--editor` said it, in which case the settings file neither
    /// overrides it nor is overwritten by it: a flag is one occasion and the
    /// box is a choice.
    editor_pinned: bool,
    status: String,
}

impl RtlScopeApp {
    pub fn new(options: ParseOptions, top: Option<String>, editor: Option<String>) -> Self {
        Self {
            open: None,
            failed: None,
            options,
            top,
            wave: None,
            sim: None,
            sim_progress: String::new(),
            sim_refused: None,
            sim_cycles: SIM_CYCLES,
            engine: rtlscope_tb::Engine::default(),
            sim_found: BTreeMap::new(),
            sim_dir: String::new(),
            python: String::new(),
            palette: crate::palette::Palette::default(),
            reload: None,
            watch: None,
            patterns: HashMap::new(),
            awaiting_reference: false,
            picker: None,
            bench: None,
            bench_sources: crate::source::Sources::default(),
            dock: Some(dock::default_layout()),
            dock_requests: Vec::new(),
            visible: BTreeSet::new(),
            floats: BTreeMap::new(),
            main_size: None,
            stim: crate::stim::StimPane::default(),
            verdict: None,
            stim_problems: Vec::new(),
            published: None,
            published_at: None,
            sim_from: Tab::Wave,
            // Undocumented, like the view knob: pinning the theme is what makes
            // a screenshot of one ground reproducible.
            theme_preference: match std::env::var("RTLSCOPE_THEME").as_deref() {
                Ok("dark") => ThemePreference::Dark,
                Ok("light") => ThemePreference::Light,
                _ => ThemePreference::System,
            },
            show_clocks: false,
            editor_pinned: editor.is_some(),
            editor_command: editor.unwrap_or_else(|| DEFAULT_EDITOR.to_string()),
            status: String::new(),
        }
    }

    /// Reads sources into a design, replacing whatever was open.
    ///
    /// `echo` also puts the diagnostics on the terminal, which is what someone
    /// who launched from a shell asked for; a drop into the window did not ask
    /// for anything there.
    pub fn load(&mut self, paths: &[PathBuf], echo: bool) {
        self.read_paths(paths, echo, Bring::Replace);
    }

    /// The same read, told whether the design it produces is meant to stand in
    /// for what was open or to be it.
    ///
    /// Only one thing turns on it, and it is the whole reason adding is not
    /// just another call to `load`: **a read that fails must not cost the
    /// reader the design they had.** Replacing, a failure is the answer — they
    /// asked for these files instead, and the reasons are what is left to show.
    /// Adding, a failure means nothing happened, and emptying the window would
    /// punish somebody for picking the wrong file with the loss of everything
    /// they had open.
    fn read_paths(&mut self, paths: &[PathBuf], echo: bool, bring: Bring) {
        if paths.is_empty() {
            return;
        }
        // A top pinned for one design is not a fact about the next. It stays
        // pinned across reads of the same sources — that is what choosing one
        // is for — but when the sources no longer hold a module by that name
        // the pin is stale, and holding the reader to it would turn every
        // later drop into "no module named `latch_check`" until a restart. That
        // is what `Infer` is, and the window is the only caller that wants it:
        // a `--top` typed at a terminal was typed for that one run.
        let rtlscope_read::Read { uir, design, diags, sources, watch, projects, unpinned } =
            rtlscope_read::read(
                paths,
                &self.options,
                self.top.as_deref(),
                rtlscope_read::StaleTop::Infer,
            );
        // Letting go of it is the window's to do: the reading says whose name
        // it dropped, and the window is what was holding the pin.
        if unpinned.is_some() {
            self.top = None;
        }

        let what = match (projects.as_slice(), paths) {
            ([project], [_]) => format!("{} (Veryl)", project.name),
            (_, [only]) => file_name(only),
            _ => format!("{} file(s)", paths.len()),
        };

        let Some(design) = design else {
            if echo {
                eprint!("{}", diags.render(&uir.files));
            }
            // Adding a file that holds a module nothing instantiates gives
            // the read a second candidate top, and "which of these two
            // designs is it?" is not a question the reader asked — they are
            // adding to the one in front of them. Measured: it is the only
            // way an ordinary add fails, because everything else the front
            // end cannot make sense of is reported and skipped rather than
            // refused. So it is answered with the top they are looking at,
            // and read again.
            //
            // Only when nothing was pinned. A top chosen on purpose is not
            // something an add gets to overrule.
            if bring == Bring::Add
                && self.top.is_none()
                && self.open.is_some()
                && diags.iter().any(|diag| diag.code == rtlscope_ir::DiagCode::TopAmbiguous)
                && let Some(top) =
                    self.open.as_ref().map(|open| open.design.top_module().base_name.clone())
            {
                self.top = Some(top.clone());
                self.read_paths(paths, echo, bring);
                match self.open.as_ref().is_some_and(|open| open.stale.is_some()) {
                    // The pin was this add's doing rather than the reader's,
                    // and it did not get them a design. It should not outlive
                    // the attempt: the next drop would be held to a name
                    // nobody chose.
                    true => self.top = None,
                    // Said, because the window is about to look exactly as it
                    // did: what was added is in the design but nothing the
                    // top reaches instantiates it, so no picture changes.
                    false => {
                        let _ = write!(self.status, " — `{top}` is still the top");
                    }
                }
                return;
            }
            // Kept whole: what was open is still open, and the reasons the
            // chosen files would not join it go where a broken edit's do.
            if bring == Bring::Add
                && let Some(previous) = self.open.take()
            {
                let errors = diags.count(Severity::Error);
                self.status = format!(
                    "these do not read together with what is open ({errors} error(s)); \
                     nothing was added"
                );
                let stale = Stale { files: uir.files, diags, from_add: true };
                self.open = Some(Open { stale: Some(stale), ..previous });
                return;
            }
            self.status = format!("`{what}` did not come out as a design");
            // Only when the top is the thing in the way. A design that failed
            // to parse has candidates too, and offering them would be offering
            // to fail again in the same place.
            let tops =
                match diags.iter().any(|diag| diag.code == rtlscope_ir::DiagCode::TopAmbiguous) {
                    true => rtlscope_elab::candidate_tops(&uir),
                    false => Vec::new(),
                };
            self.failed =
                Some(Failed { files: uir.files, diags, what, tops, paths: paths.to_vec() });
            self.open = None;
            return;
        };

        let flat = rtlscope_analyse::flat::flatten(&design);
        let top = design.top;
        let errors = diags.count(Severity::Error);
        let warnings = diags.count(Severity::Warning);
        self.status = format!(
            "{what}: {} module(s), {errors} error(s), {warnings} warning(s)",
            design.modules.len()
        );
        if let Some(top) = unpinned {
            let _ =
                write!(self.status, " — `{top}` is not in these sources, so the top was inferred");
        }
        self.failed = None;
        self.open = Some(Open {
            design,
            diags,
            flat,
            geoms: HashMap::new(),
            scene_rects: HashMap::new(),
            path: vec![tree::Crumb::top(top)],
            selected: None,
            highlighted: HashSet::new(),
            analyses: None,
            cone_graph: None,
            fsms: None,
            fsm: views::FsmPane::default(),
            depth: views::DepthPane::default(),
            pipeline_selected: 0,
            pipeline_mode: PipeMode::default(),
            source_target: None,
            walking: None,
            sources: crate::source::Sources::default(),
            source_scroll: SCROLL_FRAMES,
            source_in_bench: false,
            source_paths: paths.to_vec(),
            sources_read: sources,
            watch_paths: watch,
            trail: Vec::new(),
            cone: ConePane::default(),
            stale: None,
            stages: None,
        });

        // A recording that was already open — dropped on its own, or read
        // against the design these sources replace — now has something to be
        // named by. The rows do not change; what the design says about them
        // does.
        if let Some(open) = self.open.as_mut() {
            open.ensure_fsms();
        }
        if let Some(open) = self.open.as_ref()
            && let Some(wave) = self.wave.as_mut()
        {
            let note = wave.rebind(&open.design, &open.flat, open.state_machines());
            self.status = format!("{}; {note}", self.status);
        }
    }

    /// The tab where a waveform is drawn rather than read.
    ///
    /// Taken out and put back like the others so the drawing can be edited
    /// while the application decides what to do about it.
    fn stim_tab(&mut self, ui: &mut Ui) {
        let Some(name) = self.ensure_pattern() else {
            let why = self
                .stim_problems
                .first()
                .cloned()
                .unwrap_or_else(|| "no design is open".to_string());
            views::empty_state(ui, "nothing to draw against", &why);
            return;
        };

        let running = self.simulating().map(str::to_string);
        let refused = self.sim_refused().map(str::to_string);
        let Some(mut pattern) = self.patterns.remove(&name) else { return };
        let action = crate::stim::show(
            ui,
            &mut self.stim,
            &mut pattern,
            self.verdict.as_ref(),
            running.as_deref(),
            refused.as_deref(),
            &self.stim_problems.clone(),
        );
        // A drag paints the same value into a run of columns, which writes a
        // change per column; `tidy` turns that back into the one edge it is.
        pattern.tidy();
        self.patterns.insert(name.clone(), pattern);

        if action.save {
            self.save_pattern(&name);
        }
        if action.load {
            self.load_pattern(&name);
        }
        if action.clear {
            self.patterns.remove(&name);
            self.verdict = None;
            self.stim_problems.clear();
        }
        if action.run {
            self.start_pattern();
        }
        if let Some(column) = action.seek {
            self.seek_column(column);
        }
        // A value the port cannot hold is refused rather than narrowed, and the
        // cell simply keeps what it had — which without this reads as a cell
        // that would not take a number at all.
        if let Some(said) = action.said {
            self.status = said;
        }
    }

    /// Where a module's drawing is kept.
    ///
    /// Beside the design rather than in a scratch directory, because it is
    /// somebody's work and belongs with the RTL it is about — committable next
    /// to it, and readable by `rtlscope sim --pattern`, which is the same file.
    fn pattern_path(&self, module: &str) -> Option<PathBuf> {
        let open = self.open.as_ref()?;
        let first = open.source_paths.first()?;
        let dir = first.parent().unwrap_or(Path::new("."));
        Some(dir.join(format!("{module}_stim.json")))
    }

    /// Writes the drawing down.
    ///
    /// A drawing is the one thing in this window that is authored rather than
    /// derived — the design comes out of the source, the diagram out of the
    /// design, the waveform out of a run, but a stimulus is somebody saying
    /// what they meant to happen. Until this it lived only in memory.
    fn save_pattern(&mut self, module: &str) {
        let Some(path) = self.pattern_path(module) else {
            self.status = "no design open, so there is nowhere to put it".to_string();
            return;
        };
        let Some(pattern) = self.patterns.get(module) else { return };
        self.status = match std::fs::write(&path, pattern.to_json()) {
            Ok(()) => format!("wrote {}", path.display()),
            // Said with the path, because a checkout can be read-only and
            // "could not save" without saying where helps nobody.
            Err(why) => format!("could not write {}: {why}", path.display()),
        };
    }

    /// Reads a drawing back, squared up against the design as it is now.
    ///
    /// `reconcile` is what makes this safe across an edit: a saved drawing
    /// names ports, and a port may have been renamed, widened or taken away
    /// since. What no longer fits is reported rather than dropped.
    fn load_pattern(&mut self, module: &str) {
        let Some(path) = self.pattern_path(module) else { return };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(why) => {
                self.status = format!("could not read {}: {why}", path.display());
                return;
            }
        };
        let mut pattern = match rtlscope_tb::Pattern::from_json(&text) {
            Ok(pattern) => pattern,
            Err(why) => {
                self.status = format!("{} is not a drawing: {why}", path.display());
                return;
            }
        };

        let Some(open) = self.open.as_ref() else { return };
        let notes = match rtlscope_tb::Pattern::blank(&open.design, open.current(), STIM_COLUMNS) {
            Ok(fresh) => pattern.reconcile(&fresh),
            Err(_) => Vec::new(),
        };
        self.stim_problems = notes.clone();
        self.patterns.insert(module.to_string(), pattern);
        self.status = match notes.is_empty() {
            true => format!("read {}", path.display()),
            false => format!("read {} — {} thing(s) no longer fit", path.display(), notes.len()),
        };
    }

    /// Puts the waveform's cursor on the moment a drawn column covers.
    ///
    /// The verdict knows the time in nanoseconds because the harness recorded
    /// it while it ran, so this needs no cycle counting of its own — which is
    /// the difference between pointing at the moment and pointing near it.
    fn seek_column(&mut self, column: u64) {
        let at_ns = self
            .verdict
            .as_ref()
            .and_then(|verdict| verdict.failures.iter().find(|miss| miss.cycle == column))
            .map(|miss| miss.time_ns);
        let Some(wave) = self.wave.as_mut() else {
            self.status =
                "the waveform is not open, so there is nowhere to put the cursor".to_string();
            return;
        };
        match at_ns.and_then(|ns| wave.dump.ticks_of_ns(ns)) {
            Some(tick) => {
                wave.cursor = Some(tick);
                self.status = format!("column {column}");
                self.show(Tab::Wave);
            }
            None => self.status = format!("column {column} is not in this dump"),
        }
    }

    /// Lays out the deepest clock domain the dump recorded.
    ///
    /// Both the `stages…` button and a simulation started from the flow view
    /// end here, so the two cannot pick different domains for one dump.
    fn lay_out_open_stages(&mut self) {
        let Some(open) = self.open.as_ref() else { return };
        let design = &open.design;
        let Some(state) = self.wave.as_mut() else { return };
        lay_out_stages(design, state);
        self.status = state.status.clone();
    }

    /// Simulates the module being looked at, on another thread.
    ///
    /// The design is read again inside the thread rather than shared: elaboration
    /// is cheap next to the simulation, and sending a `Design` across would mean
    /// holding one still while the window carries on drawing from it.
    fn start_simulation(&mut self, from: Tab) {
        self.start_run(rtlscope_tb::Stimulus::Random { seed: 1 }, from);
    }

    /// Plays the drawing for the module being looked at.
    fn start_pattern(&mut self) {
        let Some(pattern) = self.current_pattern().cloned() else {
            self.status = "there is nothing drawn to play".to_string();
            return;
        };
        self.verdict = None;
        self.start_run(rtlscope_tb::Stimulus::Drawn(Box::new(pattern)), Tab::Stim);
    }

    fn start_run(&mut self, stimulus: rtlscope_tb::Stimulus, from: Tab) {
        // Written down before anything can turn back: a run that is refused —
        // no files to build from, or one already going — still says which view
        // asked, so a second attempt from the same place lands back there.
        self.sim_from = from;
        // A new attempt clears the last answer: leaving it up while the
        // spinner turns would be showing a reason for something that is still
        // being tried.
        self.sim_refused = None;
        if self.sim.is_some() {
            self.status = "a simulation is already running".to_string();
            return;
        }
        let Some(open) = self.open.as_ref() else { return };
        if open.sources_read.is_empty() {
            self.status =
                "this design was not read from files, so there is nothing to simulate".to_string();
            return;
        }

        let module = open.design.module(open.current()).base_name.clone();
        let paths = open.sources_read.clone();
        let bench = self.bench.clone();
        let cycles = self.sim_cycles;
        let options = self.options.clone();
        let top = self.top.clone();
        let engine = self.engine;
        let tools = self.tools();
        let (sender, receiver) = std::sync::mpsc::channel();

        self.sim_progress = match &bench {
            Some(bench) => format!("running `{}`…", bench.top),
            None => format!("building `{module}`…"),
        };
        self.status = self.sim_progress.clone();
        self.sim = Some(receiver);

        std::thread::spawn(move || {
            let outcome = match bench {
                // Theirs decides everything: the stimulus, how long it runs,
                // and when it stops. Nothing is generated over the top of it.
                Some(bench) => run_written_bench(&paths, &bench, engine, &tools),
                None => simulate_off_thread(
                    &paths,
                    &options,
                    top.as_deref(),
                    &module,
                    stimulus,
                    cycles,
                    engine,
                    tools,
                ),
            };
            // The window may have been closed; nobody left to tell is fine.
            let _ = sender.send(SimMessage::Done(Box::new(outcome)));
        });
    }

    /// Reads the same files again, with one module named as the top.
    ///
    /// The choice sticks: everything read afterwards — a folder dropped next,
    /// a reload after an edit — uses it too, which is what `--top` does on the
    /// command line and what somebody who has just answered the question
    /// expects not to be asked again.
    fn read_as_top(&mut self, top: String) {
        let Some(paths) = self.failed.as_ref().map(|failed| failed.paths.clone()) else { return };
        self.top = Some(top);
        self.load(&paths, false);
    }

    /// Notices the sources changing on disk, and reads them again.
    ///
    /// This is what makes the window a view of the files rather than of one
    /// moment of them: edit the RTL anywhere — an external editor, the Source
    /// tab, eventually the diagram itself — and every picture here follows,
    /// because they all derive from the same read.
    fn watch_sources(&mut self, ui: &Ui) {
        self.collect_reload(ui);

        let paths = self.open.as_ref().map(|open| open.watch_paths.clone()).unwrap_or_default();
        if paths.is_empty() {
            self.watch = None;
        } else if self.watch.as_ref().is_none_or(|watch| watch.paths != paths) {
            let (sender, changed) = std::sync::mpsc::channel();
            let ctx = ui.ctx().clone();
            let watching = paths.clone();
            std::thread::spawn(move || watch_files(watching, ctx, sender));
            self.watch = Some(SourceWatch { paths, changed });
        }

        // Only while nothing is being read: a change that arrives mid-read
        // stays in the channel and is picked up on the far side, so a save
        // during a long read is not lost and does not start a second one.
        if self.reload.is_some() {
            return;
        }
        match self.watch.as_ref().map(|watch| watch.changed.try_recv()) {
            Some(Ok(())) => self.start_reload(),
            Some(Err(TryRecvError::Disconnected)) => self.watch = None,
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    /// Reads the sources again on another thread.
    ///
    /// The same two calls `load` makes. Off the UI thread because reading a
    /// design takes real time — measured on this machine at 0.2s for a dozen
    /// files and 8s for thirty-six — and a window that stops answering for
    /// eight seconds every time a file is saved is worse than one that never
    /// noticed the save at all.
    fn start_reload(&mut self) {
        let Some(open) = self.open.as_ref() else { return };
        let paths = open.source_paths.clone();
        if paths.is_empty() {
            return;
        }

        let options = self.options.clone();
        let top = self.top.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        self.reload = Some(receiver);
        self.status = "the sources changed; reading them again…".to_string();

        std::thread::spawn(move || {
            // The same steps `load` takes, Veryl included: an edit to a
            // `.veryl` is what brought us here, and it has to be built again
            // before there is anything new to read.
            //
            // `Keep` where `load` says `Infer`: a reread that comes back
            // without a design leaves the picture already on screen standing,
            // so reading again under a different top would swap what is being
            // looked at for something nobody asked to see.
            let read = rtlscope_read::read(
                &paths,
                &options,
                top.as_deref(),
                rtlscope_read::StaleTop::Keep,
            );
            // The window may have been closed; nobody left to tell is fine.
            let _ = sender.send(Box::new(Reread {
                files: read.uir.files,
                diags: read.diags,
                design: read.design,
                sources_read: read.sources,
                watch_paths: read.watch,
            }));
        });
    }

    fn collect_reload(&mut self, ui: &Ui) {
        let Some(receiver) = self.reload.as_ref() else { return };
        match receiver.try_recv() {
            Ok(reread) => {
                self.reload = None;
                self.finish_reload(*reread);
            }
            Err(TryRecvError::Empty) => {
                ui.ctx().request_repaint_after(Duration::from_millis(150));
            }
            Err(TryRecvError::Disconnected) => {
                self.reload = None;
                self.status = "reading the sources again stopped without saying why".to_string();
            }
        }
    }

    /// Puts a re-read design on screen, with the reader where they were.
    fn finish_reload(&mut self, reread: Reread) {
        let Some(previous) = self.open.take() else { return };
        let Reread { files, diags, design, sources_read, watch_paths } = reread;

        let Some(design) = design else {
            // The picture stays. In an edit loop the sources spend a good part
            // of their time not being a design — a half-typed port list is the
            // normal case, not an emergency — and a window that empties itself
            // each time is one nobody can work in.
            let errors = diags.count(Severity::Error);
            self.status = format!(
                "the sources no longer read as a design ({errors} error(s)); \
                 showing the last version that did"
            );
            self.open =
                Some(Open { stale: Some(Stale { files, diags, from_add: false }), ..previous });
            return;
        };

        let bookmark = previous.bookmark();
        let Open { source_paths, pipeline_mode, fsm: was, .. } = previous;

        let flat = rtlscope_analyse::flat::flatten(&design);
        let restored = bookmark.resolve(&design, &design.files);
        let (modules, errors, warnings) =
            (design.modules.len(), diags.count(Severity::Error), diags.count(Severity::Warning));
        let mut lost = restored.lost;

        let mut open = Open {
            design,
            diags,
            flat,
            geoms: HashMap::new(),
            scene_rects: restored.camera,
            path: restored.path,
            walking: None,
            depth: views::DepthPane::default(),
            selected: None,
            highlighted: restored.highlighted,
            analyses: None,
            cone_graph: None,
            fsms: None,
            fsm: views::FsmPane::with_mode(was.mode),
            pipeline_selected: bookmark.pipeline_selected,
            pipeline_mode,
            source_target: restored.source,
            sources: crate::source::Sources::default(),
            source_scroll: SCROLL_FRAMES,
            source_in_bench: false,
            source_paths,
            sources_read,
            watch_paths,
            trail: restored.trail,
            cone: ConePane::default(),
            stale: None,
            stages: None,
        };

        // The two that need something built to be found again: a box exists
        // only once its module is laid out, a machine only once the analyses
        // have run. Both are about to be needed anyway.
        if let Some(label) = &bookmark.selected {
            let module = open.current();
            let found = open
                .geom(module)
                .boxes
                .iter()
                .find(|item| item.label == *label)
                .map(|item| (item.node, item.span));
            match found {
                Some((node, span)) => {
                    open.selected = Some(Selected { node, label: label.clone(), span });
                }
                None => lost.push(format!("`{label}` is not in the diagram any more")),
            }
        }
        if let Some(name) = &bookmark.fsm {
            open.ensure_fsms();
            let machines = open.state_machines();
            let at = machines
                .iter()
                .position(|m| format!("{}.{}", m.module_name, m.state_name) == *name);
            match at {
                Some(at) => open.fsm.selected = at,
                None => lost.push(format!("the machine `{name}` is gone")),
            }
        }

        // The dump did not change; what the design says about it did.
        open.ensure_fsms();
        let note = self
            .wave
            .as_mut()
            .map(|state| state.rebind(&open.design, &open.flat, open.state_machines()));
        let mut status =
            format!("read again: {modules} module(s), {errors} error(s), {warnings} warning(s)");
        if let Some(note) = note {
            status.push_str(&format!("; {note}"));
        }
        if !lost.is_empty() {
            status.push_str(&format!(". Could not put back: {}", lost.join("; ")));
        }

        self.status = status;
        self.failed = None;
        self.open = Some(open);
    }

    /// Says which design this window has open, where the MCP server looks.
    ///
    /// Called from the frame loop and nowhere else, because what the note
    /// asserts is that a *window* has this open — reading a design headlessly,
    /// as a test does, must not claim one. Rewritten when the design changes,
    /// and again every couple of minutes so that the age the note carries
    /// distinguishes a window still open from one that was killed and never
    /// took its note down.
    ///
    /// Failing to write it is not worth interrupting anyone over: the MCP
    /// server asks for files when there is no note. So it goes to the status
    /// line and no further.
    fn publish_session(&mut self) {
        let Some(open) = self.open.as_ref() else { return };
        if open.source_paths.is_empty() {
            return;
        }
        let same = self.published.as_ref() == Some(&open.source_paths);
        let fresh = self.published_at.is_some_and(|at| at.elapsed() < SESSION_REFRESH);
        if same && fresh {
            return;
        }

        let session = rtlscope_sv::Session::of(
            &rtlscope_sv::session::absolute(&open.source_paths),
            self.top.as_deref(),
            &self.options,
        );
        match session.write() {
            Ok(_) => {
                self.published = Some(open.source_paths.clone());
                self.published_at = Some(Instant::now());
            }
            Err(error) => {
                self.status = format!("{}; could not say so to rtlscope-mcp: {error}", self.status);
                // Not retried every frame: it will not start working on its own.
                self.published_at = Some(Instant::now());
            }
        }
    }

    /// The name of the module a drawing would be against.
    fn drawing_for(&self) -> Option<String> {
        let open = self.open.as_ref()?;
        Some(open.design.module(open.current()).base_name.clone())
    }

    fn current_pattern(&self) -> Option<&rtlscope_tb::Pattern> {
        self.patterns.get(&self.drawing_for()?)
    }

    /// The drawing for the module being looked at, made blank if there is none
    /// and squared up with the design if there is.
    ///
    /// Squaring up matters because a drawing outlives the sources being read
    /// again: a row whose port has gone would otherwise be driven into a
    /// harness that cannot compile, and the error would arrive from a simulator
    /// rather than from here.
    fn ensure_pattern(&mut self) -> Option<String> {
        let (open, name) = (self.open.as_ref()?, self.drawing_for()?);
        let module = open.current();
        let fresh = match rtlscope_tb::Pattern::blank(&open.design, module, STIM_COLUMNS) {
            Ok(fresh) => fresh,
            Err(why) => {
                self.stim_problems = vec![why.to_string()];
                return None;
            }
        };
        match self.patterns.get_mut(&name) {
            Some(mine) => {
                let lost = mine.reconcile(&fresh);
                if !lost.is_empty() {
                    self.stim_problems = lost;
                }
            }
            None => {
                self.patterns.insert(name.clone(), fresh);
                self.stim_problems.clear();
            }
        }
        Some(name)
    }

    /// Picks up whatever the simulation thread has said.
    fn collect_simulation(&mut self, ui: &Ui) {
        let Some(receiver) = self.sim.as_ref() else { return };
        match receiver.try_recv() {
            Ok(SimMessage::Progress(what)) => {
                self.sim_progress = what.clone();
                self.status = what;
            }
            Ok(SimMessage::Done(result)) => {
                self.sim = None;
                self.sim_progress.clear();
                match *result {
                    Ok(ran) => {
                        self.sim_refused = None;
                        if let Some(verdict) = ran.verdict {
                            self.status = verdict.summary();
                            self.verdict = Some(verdict);
                        }
                        let how = ran.how.clone();
                        self.open_dump(ran.dump);
                        // After the dump, so the view that asked wins over the
                        // waveform the dump itself brings up.
                        self.show(self.sim_from);
                        // Asked for from the flow view, the intent was to see
                        // the flow — not to be handed a dump and a second
                        // button to press.
                        if self.sim_from == Tab::Pipeline {
                            self.lay_out_open_stages();
                        }
                        // Last of all. Everything above writes a status of its
                        // own — the dump's summary, then the stages' — and this
                        // belongs on the end of whichever had the final word.
                        self.status.push_str(&format!("; simulated with {how}"));
                    }
                    Err(why) => {
                        self.status = why.clone();
                        self.sim_refused = Some(why);
                    }
                }
            }
            Err(TryRecvError::Empty) => {
                // Nothing yet. Keep the frames coming so the spinner turns and
                // the answer is noticed when it arrives.
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
            }
            Err(TryRecvError::Disconnected) => {
                self.sim = None;
                self.sim_progress.clear();
                self.status = "the simulation stopped without saying why".to_string();
            }
        }
    }

    /// What the wave and flow views show while one is running.
    fn simulating(&self) -> Option<&str> {
        self.sim.is_some().then_some(self.sim_progress.as_str())
    }

    /// Why the last simulation produced nothing, if it produced nothing.
    ///
    /// Kept apart from the status line. The status line is one row of small
    /// grey text along the bottom of a window that can be two thousand pixels
    /// wide, which is the right place for "23 modules read" and the wrong
    /// place for the answer to a button somebody just pressed — measured, by
    /// pressing it and not finding out.
    fn sim_refused(&self) -> Option<&str> {
        self.sim_refused.as_deref()
    }

    /// Which way the Pipeline tab reads, by name. Undocumented, like the tab.
    pub fn show_pipe_mode(&mut self, name: &str) {
        if let Some(open) = self.open.as_mut() {
            open.pipeline_mode = match name {
                "flow" => PipeMode::Flow,
                "structure" => PipeMode::Structure,
                _ => return,
            };
        }
    }

    /// Draws a demonstration pattern, without a mouse.
    ///
    /// Undocumented, like the other knobs: a grid nobody can fill in from
    /// outside is a grid nobody can photograph.
    pub fn draw_demo(&mut self) {
        let Some(name) = self.ensure_pattern() else { return };
        let Some(pattern) = self.patterns.get_mut(&name) else { return };
        for (index, lane) in pattern.drive.iter_mut().enumerate() {
            let value = match lane.width {
                1 => 1,
                _ => 0x10 + index as u64,
            };
            lane.set(2, rtlscope_tb::Cell::Value(value));
            lane.set(3, rtlscope_tb::Cell::Value(0));
            lane.set(6, rtlscope_tb::Cell::Value(value));
            lane.set(9, rtlscope_tb::Cell::DontCare);
        }
        if let Some(lane) = pattern.expect.first_mut() {
            lane.set(5, rtlscope_tb::Cell::Value(1));
            lane.set(6, rtlscope_tb::Cell::Value(0));
        }
        pattern.tidy();
    }

    /// Writes the track names one way or the other, without a click.
    ///
    /// Undocumented, like the other knobs. The toggle is a button, and a state
    /// that only exists after a button has been pressed cannot be
    /// photographed from a script — which is the whole reason these exist.
    pub fn name_style(&mut self, how: &str) {
        let Some(wave) = self.wave.as_mut() else { return };
        wave.names = match how {
            "path" | "full" | "flat" => crate::wave::Names::Path,
            _ => crate::wave::Names::Tree,
        };
    }

    /// Selects a track and marks a moment, without a mouse.
    ///
    /// Undocumented, like the other knobs: a ruler with nothing on it and a
    /// measurement with nothing to measure are not what the panel looks like in
    /// use, and that is the state worth photographing.
    pub fn measure_demo(&mut self) {
        let Some(wave) = self.wave.as_mut() else { return };
        let names = wave.matched_names();
        let picked: Vec<String> = names.iter().take(4).map(|name| (*name).to_string()).collect();
        for name in picked {
            let _ = wave.add_by_ir_name(&name);
        }
        wave.select_only(1);
        let end = wave.dump.max_time().max(1);
        wave.cursor = Some(end / 3);
        wave.toggle_marker(end / 6);
        wave.toggle_marker(end / 2);
        wave.refit = true;
    }

    /// Goes to the first moment two recordings part, without a click.
    /// Undocumented, like the other knobs.
    pub fn seek_first_difference(&mut self) {
        let Some(wave) = self.wave.as_mut() else { return };
        wave.seek_first_difference(1000.0);
        self.status = wave.status.clone();
    }

    /// Opens the signal picker, with a search already in it. Undocumented,
    /// like the other knobs: a panel that only appears after a button is
    /// pressed is a panel nothing outside the window can check.
    pub fn pick_signals(&mut self, needle: &str) {
        let Some(wave) = self.wave.as_mut() else { return };
        wave.open_picker(needle);
    }

    /// Opens the cone on a named signal. Undocumented, like the other knobs.
    ///
    /// `RTLSCOPE_CONE=out_data` or `RTLSCOPE_CONE=loads:in_valid`. It exists
    /// because the Trace tab's two modes and its root are three clicks deep,
    /// and a drawing nobody can reach from a script is a drawing nobody can
    /// screenshot — which is how a resizing bug in it went unnoticed until a
    /// reader hit it.
    pub fn show_cone(&mut self, spec: &str) {
        let (towards, want) = match spec.split_once(':') {
            Some(("loads", name)) => (rtlscope_analyse::cone::Towards::Loads, name),
            Some(("drivers", name)) => (rtlscope_analyse::cone::Towards::Drivers, name),
            _ => (rtlscope_analyse::cone::Towards::Drivers, spec),
        };
        let want = want.trim();
        let Some(mut open) = self.open.take() else { return };

        let found = open
            .flat
            .all_names(&open.design)
            .find(|(name, ..)| name == want || name.ends_with(&format!(".{want}")))
            .map(|(_, signal, ..)| signal);
        match found.and_then(|signal| home_of_signal(&open, signal)) {
            Some((_, net)) => {
                open.cone.drawn = true;
                open.cone.towards = towards;
                self.pick_net(&mut open, net);
                // After picking, not before: picking a wire moves to the source
                // to show where it is declared, which would otherwise put the
                // reader on a view they did not ask for.
                self.show(Tab::Trace);
            }
            None => self.status = format!("no signal called `{want}` in this design"),
        }
        self.open = Some(open);
    }

    /// Gives windows to views named by name, for looking at without clicking.
    ///
    /// Reports what it did not recognise rather than starting a window short
    /// and saying nothing, so a screenshot taken through this knob cannot show
    /// one window while the caller believes it asked for two.
    pub fn pop_out_named(&mut self, names: &str) {
        let mut unknown = Vec::new();
        for name in names.split(',').map(str::trim).filter(|name| !name.is_empty()) {
            match Tab::of(name) {
                Some(tab) => self.detach(tab),
                None => unknown.push(name.to_string()),
            }
        }
        if !unknown.is_empty() {
            self.status = format!("no such view: {}", unknown.join(", "));
        }
    }

    /// Writes the drawing down without a click. Undocumented, like the others.
    pub fn save_drawing(&mut self) {
        let Some(name) = self.drawing_for() else { return };
        self.save_pattern(&name);
    }

    /// Plays the drawing without a click, so the loop can be photographed.
    pub fn play_drawing(&mut self) {
        self.start_pattern();
    }

    /// Which way the FSM tab is looking at its machine. Undocumented, like the
    /// other knobs: a toggle cannot be pressed from outside, and a view nobody
    /// can reach without a click is a view nobody checks.
    pub fn show_fsm_mode(&mut self, name: &str) {
        if let Some(open) = self.open.as_mut() {
            open.fsm.mode = match name {
                "diagram" => views::FsmMode::Diagram,
                "list" => views::FsmMode::List,
                _ => return,
            };
        }
    }

    /// Starts a simulation without a click. Undocumented, like the view knob:
    /// a button cannot be pressed from outside, and a view that only exists
    /// after a press is one nobody can check.
    pub fn simulate_now(&mut self) {
        // As if pressed on whichever view is in front, so the answer comes back
        // there — the same thing a click would have done.
        self.start_simulation(self.focused_tab().unwrap_or(Tab::Wave));
    }

    /// Which tab to show, by name.
    ///
    /// Applied after everything has been opened, since opening a dump moves to
    /// the wave tab of its own accord and would otherwise win.
    pub fn show_tab(&mut self, name: &str) {
        // Through the same table a saved layout uses. This once kept a second
        // list of its own, which had already drifted: it had no name for
        // Diagnostics at all, and a typo was indistinguishable from asking for
        // the tab that was showing anyway.
        match Tab::of(name) {
            Some(tab) => self.show(tab),
            None => self.status = format!("no such view: {name}"),
        }
    }

    /// Walks a provenance trail, without a mouse.
    ///
    /// Undocumented, like the other knobs here: clicking a wire is a pointer
    /// gesture, and a view that only exists after one cannot be checked from
    /// outside. `RTLSCOPE_TRACE=<signal>[,<signal>...]` puts the window where
    /// that click and the `follow` presses after it would have left it. Each
    /// name after the first is followed within the same module, which is what
    /// `follow` does; crossing an instance boundary needs the pointer.
    pub fn trace_signal(&mut self, signals: &str) {
        for (step, name) in signals.split(',').map(str::trim).filter(|s| !s.is_empty()).enumerate()
        {
            let Some(mut open) = self.open.take() else { return };
            let module = open.trail.last().map_or_else(|| open.current(), |hop| hop.module());
            let found = open
                .design
                .module(module)
                .nets
                .iter_enumerated()
                .find(|(_, net)| net.name == name)
                .map(|(id, _)| id);
            let Some(net) = found else {
                self.status = format!("no signal `{name}` there");
                self.open = Some(open);
                return;
            };
            match step {
                0 => self.pick_net(&mut open, net),
                _ => {
                    let path = open.trail.last().map(|hop| hop.path.clone()).unwrap_or_default();
                    self.hop(&mut open, views::Hop { path, net });
                }
            }
            self.open = Some(open);
        }
    }

    /// Opens the stages and puts the cursor on a cycle, without a mouse.
    ///
    /// Undocumented, like RTLSCOPE_TAB: a pointer cannot be scripted from
    /// outside, and a picture that only exists after two clicks is one nobody
    /// can check. `RTLSCOPE_CYCLE=<n>` puts the window in the state those clicks
    /// would have left it in.
    pub fn seek_cycle(&mut self, cycle: usize) {
        let Some(open) = self.open.as_ref() else { return };
        let report = rtlscope_analyse::pipeline::analyse(&open.design);
        let Some(wave) = self.wave.as_mut() else { return };
        let Some(domain) =
            report.domains.into_iter().find(|d| wave.matches.by_ir_name(&d.clock).is_some())
        else {
            return;
        };
        wave.open_stages(domain);
        if let Some(at) = wave.cycle_time(cycle) {
            wave.cursor = Some(at);
        }
        self.status = wave.status.clone();
    }

    /// Opens a dump, which is how the wave tab gets something to show.
    ///
    /// With or without a design. A recording carries its own hierarchy, its own
    /// times and its own values, so `rtlscope-gui run.vcd` — or a `.vcd` dragged
    /// onto an empty window — opens the viewer on it. What the design adds is
    /// the naming: which net a row is, and so the way back to the diagram, the
    /// source and the cone. Sources read later are matched against the
    /// recording that is already open, so arriving in the other order costs
    /// nothing.
    pub fn open_dump(&mut self, path: PathBuf) {
        // A dump arriving while `compare…` is armed is the second recording,
        // not a replacement for the first. The flag is cleared either way: a
        // request that went nowhere must not sit waiting to surprise the next
        // drop.
        if std::mem::take(&mut self.awaiting_reference)
            && let Some(wave) = self.wave.as_mut()
        {
            let name = file_name(&path);
            self.status = match wave.open_reference(path) {
                Ok(()) => wave.status.clone(),
                Err(error) => format!("`{name}` could not be read: {error}"),
            };
            return;
        }

        // Before the borrow below, and before the file is read: naming a state
        // register's values is part of lining a dump up with a design, so the
        // machines have to be found by the time it opens.
        if let Some(open) = self.open.as_mut() {
            open.ensure_fsms();
        }
        let against =
            self.open.as_ref().map(|open| (&open.design, &open.flat, open.state_machines()));
        match crate::wave::WaveState::open(path, against) {
            Ok(state) => {
                self.status = state.status.clone();
                self.wave = Some(state);
                // Brought up where the other reading is done. A waveform wants
                // width for time and height for signals, and the group along
                // the bottom is short of both — so this is a starting place and
                // not a verdict: dragged into a pane of its own, or out into a
                // window that floats over the rest, that is where it opens next
                // time.
                self.show(Tab::Wave);
            }
            Err(error) => self.status = format!("{error}"),
        }
    }

    /// Opens a second recording to hold the open one against.
    ///
    /// Separate from [`RtlScopeApp::open_dump`] because it is a different act:
    /// the first dump decides what the panel is showing, the second only says
    /// what to measure it against.
    pub fn compare_against(&mut self, path: PathBuf) {
        let name = file_name(&path);
        let Some(wave) = self.wave.as_mut() else {
            self.status = format!("`{name}` has nothing to compare against — open a dump first");
            return;
        };
        self.status = match wave.open_reference(path) {
            Ok(()) => wave.status.clone(),
            Err(error) => format!("`{name}` could not be read: {error}"),
        };
    }

    /// A results file, read into the wave state that is already open.
    ///
    /// It is only useful beside a waveform — the moments in it are places to
    /// put the cursor — so without one open there is nothing to do with it.
    pub fn open_results(&mut self, path: PathBuf) {
        let Some(wave) = self.wave.as_mut() else {
            self.status =
                "open a dump first: a results file is a set of moments in one".to_string();
            return;
        };
        match rtlscope_tb::results::read(&path) {
            Ok(run) => {
                let (passed, failed, skipped) = run.counts();
                self.status = format!("{passed} passed, {failed} failed, {skipped} skipped");
                wave.status = self.status.clone();
                wave.results = Some(run);
                self.show(Tab::Wave);
            }
            Err(error) => self.status = format!("{error}"),
        }
    }

    /// Whatever was dragged onto the window.
    fn take_dropped(&mut self, ui: &Ui) {
        let dropped: Vec<PathBuf> = ui.input(|input| {
            input.raw.dropped_files.iter().filter_map(|file| file.path.clone()).collect()
        });
        if !dropped.is_empty() {
            // A drop replaces. Dropping one project on a window showing
            // another means opening it, and a drop that quietly welded two
            // unrelated designs together would leave no way to say the
            // ordinary thing. Adding is asked for by name, in the `open` menu.
            self.open_paths(dropped, Bring::Replace);
        }
    }

    /// Whatever arrived — by drop, from a dialog, or as a sample — sorted by
    /// what it is and opened in the order that makes sense: sources first,
    /// since a waveform is read against them, and results last, since they
    /// are moments in a waveform.
    ///
    /// One way in for the three ways files arrive, so a dialog can never come
    /// to treat a `.vcd` differently from a drop of the same file.
    ///
    /// [`Bring::Add`] reads what arrives together with what is already open
    /// rather than instead of it. Only the sources join: a waveform and a
    /// results file are read against whatever the design turns out to be, and
    /// there is no sense in which a second recording joins the first.
    pub fn open_paths(&mut self, paths: Vec<PathBuf>, bring: Bring) {
        let (mut sources, mut dumps, mut results, mut unknown) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut folders: Vec<String> = Vec::new();
        for path in paths {
            match Dropped::of(&path) {
                Dropped::Source => sources.push(path),
                Dropped::FileList => match crate::read_file_list(&path) {
                    Ok(listed) => sources.extend(listed),
                    Err(error) => self.status = format!("{error:#}"),
                },
                Dropped::Dump => dumps.push(path),
                Dropped::Results => results.push(path),
                // A real project is a tree of a few hundred files, and adding
                // them one at a time is not something anybody does twice.
                // A Veryl project is opened by its manifest, and Veryl says
                // which files it holds. Walking the tree instead would find
                // the SystemVerilog it wrote last time, beside whatever else
                // is in there — the standard library it emits, for one.
                Dropped::Folder if rtlscope_veryl::project_of(&path).is_some() => {
                    let project = rtlscope_veryl::project_of(&path).expect("just checked");
                    folders.push(format!(
                        "Veryl project `{}` in {}",
                        project.name,
                        file_name(&path)
                    ));
                    sources.push(project.manifest);
                }
                Dropped::Folder => {
                    let found = gather_sources(&path);
                    folders.push(describe(&path, &found));
                    sources.extend(found.sources);
                    // Onto the options before the load, since that is what the
                    // parser is handed.
                    for directory in found.includes {
                        if !self.options.include_paths.contains(&directory) {
                            self.options.include_paths.push(directory);
                        }
                    }
                }
                Dropped::Unknown => unknown.push(file_name(&path)),
            }
        }

        // Joining, what is open goes first and what arrived follows. The order
        // is not cosmetic: a compiler reads a file list in order and a
        // declaration has to arrive before its use, so the files that already
        // read as a design keep the positions they read in.
        let mut added = 0;
        let mut already = 0;
        // A testbench never reaches here; `collect_picked` sends it elsewhere.
        // Said rather than left to `_`, so that adding a third way in cannot
        // quietly turn one into a design.
        debug_assert_ne!(bring, Bring::Testbench, "a testbench is not read as a design");
        let sources = match (bring, self.open.as_ref()) {
            (Bring::Add, Some(open)) => {
                let same =
                    |path: &Path| dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                let had: HashSet<PathBuf> = open.source_paths.iter().map(|p| same(p)).collect();
                let mut joined = open.source_paths.clone();
                for path in sources {
                    // A file already in the design is not added again. Reading
                    // one twice declares every module in it twice, and the
                    // reader would be shown a pile of redefinitions for having
                    // picked a file that was already there.
                    match had.contains(&same(&path)) {
                        true => already += 1,
                        false => {
                            joined.push(path);
                            added += 1;
                        }
                    }
                }
                joined
            }
            _ => sources,
        };

        // Nothing new to read: say so rather than reading the same files again
        // and reporting it as though something had happened.
        if bring == Bring::Add && added == 0 && already > 0 {
            self.status = match already {
                1 => "that file is already in this design".to_string(),
                n => format!("those {n} files are already in this design"),
            };
        } else {
            self.read_paths(&sources, false, bring);
        }
        // After the load, so it has the last word: what came out of the folder
        // is the thing the reader has to check when the design is not what they
        // expected, and it is a sentence they cannot get any other way.
        if !folders.is_empty() {
            self.status = folders.join("; ");
        }
        // And after that, because how many files joined is the answer to what
        // was just asked. Only when the read came to something: a failed add
        // has already said that nothing was added, and a count in front of it
        // would read as though some of them had gone in.
        if added > 0 && self.open.as_ref().is_some_and(|open| open.stale.is_none()) {
            let skipped = match already {
                0 => String::new(),
                n => format!(" ({n} already there)"),
            };
            self.status = format!("added {added} file(s){skipped} — {}", self.status);
        }
        for dump in dumps {
            self.open_dump(dump);
        }
        for file in results {
            self.open_results(file);
        }
        if !unknown.is_empty() && sources.is_empty() {
            self.status = format!(
                "{} is not something RTLScope reads: sources (.sv .v .veryl), a folder of them or \
                 a Veryl project, a file list (.f), a waveform (.vcd .fst) or a run's results.xml",
                unknown.join(", ")
            );
        }
    }

    /// Reads files as a testbench rather than as a design.
    ///
    /// Nothing about the design changes: this is a second attribute of the
    /// same session, and the views go on showing what they showed. What
    /// changes is what `simulate` runs.
    pub fn open_testbench(&mut self, paths: Vec<PathBuf>) {
        let files: Vec<PathBuf> = paths
            .iter()
            .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.clone()))
            .collect();
        let (uir, _) = rtlscope_sv::lower_files(&files, &self.options);
        match rtlscope_tb::bench::read(&files, &uir, None) {
            Ok(bench) => {
                self.status = format!(
                    "testbench `{}` — {}",
                    bench.top,
                    bench.notes.first().cloned().unwrap_or_default()
                );
                self.bench = Some(bench);
                // A new table, so whatever the old one's ids pointed at is
                // not what these do.
                self.bench_sources = crate::source::Sources::default();
            }
            Err(error) => {
                // Kept, whatever it was: a file that is not a testbench has
                // not replaced the one that is.
                self.status = format!("{error}");
            }
        }
    }

    /// Puts the testbench down again, and goes back to a generated harness.
    pub fn drop_testbench(&mut self) {
        match self.bench.take() {
            Some(bench) => {
                self.status =
                    format!("`{}` put down; simulating generates a harness again", bench.top)
            }
            None => self.status = "no testbench was open".to_string(),
        }
    }

    /// The testbench in use, for a view that has to say so.
    pub fn testbench(&self) -> Option<&rtlscope_tb::Bench> {
        self.bench.as_ref()
    }

    /// Asks the system for files, in a dialog of its own.
    ///
    /// The operating system's dialog — the one every other program on the
    /// machine opens — rather than one drawn here, because the reader already
    /// knows where their files are in it. It runs on a thread of its own: a
    /// dialog is modal and pumps its own messages for as long as it is up,
    /// and this thread has windows to keep drawing.
    fn browse(&mut self, ctx: &egui::Context, what: views::Browse) {
        if self.picker.is_some() {
            self.status = "a file dialog is already open".to_string();
            return;
        }
        // Beside the open design, when there is one; a waveform is usually a
        // folder away from the sources it recorded.
        let start_in = self
            .open
            .as_ref()
            .and_then(|open| open.source_paths.first())
            .and_then(|path| path.parent())
            .map(Path::to_path_buf);
        let bring = match what {
            views::Browse::AddFiles | views::Browse::AddFolder => Bring::Add,
            views::Browse::Testbench => Bring::Testbench,
            _ => Bring::Replace,
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if let Some(dir) = start_in {
                dialog = dialog.set_directory(dir);
            }
            // The filter is the help: a dialog showing every file on the disk
            // has said nothing about what this program reads.
            let picked: Vec<PathBuf> = match what {
                views::Browse::Files | views::Browse::AddFiles => dialog
                    .set_title(match what {
                        views::Browse::AddFiles => "Add sources to this design",
                        _ => "Open sources",
                    })
                    .add_filter("SystemVerilog", &["sv", "v", "svh", "vh", "f"])
                    .add_filter("Veryl", &["veryl", "toml"])
                    .add_filter(
                        "Anything RTLScope reads",
                        &["sv", "v", "svh", "vh", "f", "veryl", "toml", "vcd", "fst", "xml"],
                    )
                    .add_filter("All files", &["*"])
                    .pick_files()
                    .unwrap_or_default(),
                views::Browse::Folder | views::Browse::AddFolder => {
                    let title = match what {
                        views::Browse::AddFolder => "Add a folder to this design",
                        _ => "Open a project folder",
                    };
                    dialog.set_title(title).pick_folder().into_iter().collect()
                }
                views::Browse::Waveform => dialog
                    .set_title("Open a waveform")
                    .add_filter("Waveform", &["vcd", "fst"])
                    .pick_file()
                    .into_iter()
                    .collect(),
                views::Browse::Results => dialog
                    .set_title("Open a run's results")
                    .add_filter("cocotb results", &["xml"])
                    .pick_file()
                    .into_iter()
                    .collect(),
                views::Browse::Testbench => dialog
                    .set_title("Open a testbench you wrote")
                    .add_filter("SystemVerilog", &["sv", "v", "svh", "vh"])
                    .add_filter("All files", &["*"])
                    .pick_files()
                    .unwrap_or_default(),
            };
            // A cancelled dialog sends nothing worth acting on, but sends it,
            // so the window knows the dialog has gone. What was asked travels
            // with the answer: the reply arrives frames later, and a flag left
            // beside it would be a second thing to keep in step.
            let _ = sender.send((bring, picked));
            ctx.request_repaint();
        });
        self.picker = Some(receiver);
    }

    /// What the dialog came back with, opened the way a drop would be.
    fn collect_picked(&mut self) {
        let Some(receiver) = self.picker.as_ref() else { return };
        match receiver.try_recv() {
            Ok((bring, picked)) => {
                self.picker = None;
                // Cancelled means nothing to say: the status still holds
                // whatever asked for the dialog, which may have said "or drop
                // it in".
                if !picked.is_empty() {
                    match bring {
                        // Not sorted the way a drop is: these were asked for
                        // as a testbench, and a `.sv` picked in that dialog is
                        // not a design file that happens to be in the way.
                        Bring::Testbench => self.open_testbench(picked),
                        _ => self.open_paths(picked, bring),
                    }
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.picker = None,
        }
    }

    /// Opens one of the designs that ship inside the window.
    pub fn open_sample(&mut self, id: &str) {
        match crate::samples::home() {
            Some(home) => self.open_sample_from(id, &home),
            None => self.status = "this machine names no place to put a sample".to_string(),
        }
    }

    /// The same, written under a named folder.
    ///
    /// Separate so a test can open one without writing into the reader's own
    /// settings.
    fn open_sample_from(&mut self, id: &str, home: &Path) {
        let Some(sample) = crate::samples::find(id) else {
            self.status = format!("no sample called `{id}`");
            return;
        };
        let written = match sample.write(home) {
            Ok(written) => written,
            Err(error) => {
                self.status = format!("could not write the sample: {error}");
                return;
            }
        };
        // The top this design needs, in place of whatever was pinned for the
        // last one. A sample is a known design, so the right top is a fact
        // about it and not a choice left to the reader.
        self.top = sample.top.map(str::to_string);
        self.open_paths(written.open, Bring::Replace);
        self.watch_top(sample.watch);
        if let Some(tab) = sample.tab {
            self.show_tab(tab);
        }
        // The testbench that came with it, if one did — loaded exactly as one
        // the reader picked would be, so that `simulate` runs it. After the
        // design, because a replaced design puts any testbench down, and this
        // one belongs to the design that was just opened.
        if let Some(bench) = sample.bench {
            let opened = std::mem::take(&mut self.status);
            self.open_testbench(vec![written.dir.join(bench)]);
            // Both halves of what happened: what was opened, and that its
            // testbench is in hand.
            self.status = match self.bench.as_ref() {
                Some(bench) => format!("{opened} · testbench `{}` loaded", bench.top),
                None => format!("{opened} · {}", self.status),
            };
        }
        // Last, so it survives whatever opening said. A sample is the one
        // design the reader did not put on disk themselves, and where it went
        // is the thing they cannot otherwise find out — and whether this open
        // wrote anything, since a reader who edited the sample wants to know
        // that their edit was just put back.
        let _ = match written.changed {
            0 => write!(self.status, " — in {}", written.dir.display()),
            n => write!(self.status, " — {n} file(s) written to {}", written.dir.display()),
        };
    }

    /// Puts nets on the waveform by name: the top's own, or `instance.net` for
    /// one inside.
    ///
    /// What a sample's recording was made to show, so it is showing when the
    /// window opens rather than behind a picker: a bus is twenty signals, and a
    /// reader who has to find each one before the first handshake is visible
    /// has been handed a recording rather than shown one. Names the design
    /// does not have are skipped — the list belongs to the sample, and a
    /// sample is tested to open.
    fn watch_top(&mut self, names: &[&str]) {
        if names.is_empty() || self.wave.is_none() {
            return;
        }
        let Some(mut open) = self.open.take() else { return };
        // Whatever opening said is the sentence worth keeping; each track
        // added would otherwise overwrite it with its own.
        let status = std::mem::take(&mut self.status);
        for full in names {
            // `u_master.state` is a net of the module under `u_master`; a bare
            // name is one of the top's own. Either way the instance path is
            // what the dump knows the signal by, and what the track keeps so
            // it can be followed back to the diagram.
            let (path, name) = match full.rsplit_once('.') {
                Some((path, name)) => (path, name),
                None => ("", *full),
            };
            let Some(node) = open.flat.nodes.iter().find(|node| node.path == path) else {
                continue;
            };
            let module = node.module;
            let found = open
                .design
                .module(module)
                .nets
                .indices()
                .find(|id| open.design.module(module).net(*id).name == name);
            if let Some(net) = found {
                let crumbs = tree::crumbs_for(&open.design, path);
                self.watch(&mut open, &crumbs, net);
            }
        }
        self.status = status;
        self.open = Some(open);
    }

    /// The status line, for the shell that asked for a sample.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// The `open` menu: what a drop takes, chosen in a dialog instead, and
    /// the designs that ship inside the window.
    ///
    /// In the toolbar rather than on the welcome screen, because the welcome
    /// screen is gone the moment something is open and the reader still has
    /// to be able to open the next thing. It used to be in both, and the
    /// copy on the welcome screen was a second list of the same designs to
    /// keep in step with this one.
    fn open_menu(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut browse: Option<views::Browse> = None;
        let mut sample: Option<&'static str> = None;
        // Read before the menu is built: the closure borrows `ui`, and asking
        // `self` anything inside it would borrow that too.
        let has_design = self.open.is_some();
        ui.menu_button("open", |ui| {
            if ui
                .button("files…")
                .on_hover_text("Sources, a file list, a waveform or a results file  (Ctrl+O)")
                .clicked()
            {
                browse = Some(views::Browse::Files);
                ui.close();
            }
            if ui
                .button("a folder…")
                .on_hover_text(
                    "Every .sv and .v under it; folders holding headers go on the include path",
                )
                .clicked()
            {
                browse = Some(views::Browse::Folder);
                ui.close();
            }
            if ui
                .button("a waveform…")
                .on_hover_text("A .vcd or .fst, with or without a design open")
                .clicked()
            {
                browse = Some(views::Browse::Waveform);
                ui.close();
            }
            if ui
                .button("results…")
                .on_hover_text("A cocotb results.xml, read into the open waveform")
                .clicked()
            {
                browse = Some(views::Browse::Results);
                ui.close();
            }
            // Only with something open: before that, adding to nothing and
            // opening are the same act, and a menu offering both would be
            // asking the reader to tell apart two entries that do one thing.
            if has_design {
                ui.separator();
                if ui
                    .button("a testbench…")
                    .on_hover_text(
                        "One you wrote. It is read beside the design rather than as part \
                         of it, and `simulate` runs yours instead of generating a harness.",
                    )
                    .clicked()
                {
                    browse = Some(views::Browse::Testbench);
                    ui.close();
                }
                if ui
                    .button("add sources…")
                    .on_hover_text(
                        "Read them together with what is open, instead of instead of it — \
                         for a design whose parts live in more than one place",
                    )
                    .clicked()
                {
                    browse = Some(views::Browse::AddFiles);
                    ui.close();
                }
                if ui
                    .button("add a folder…")
                    .on_hover_text("Every .sv and .v under it, read together with what is open")
                    .clicked()
                {
                    browse = Some(views::Browse::AddFolder);
                    ui.close();
                }
            }
            ui.separator();
            ui.menu_button("a sample", |ui| {
                for candidate in crate::samples::ALL {
                    if ui
                        .button(RichText::new(candidate.id).monospace())
                        .on_hover_text(candidate.what)
                        .clicked()
                    {
                        sample = Some(candidate.id);
                        ui.close();
                    }
                }
            });
        })
        .response
        .on_hover_text("Open something without dragging it in");
        if let Some(what) = browse {
            self.browse(&ctx, what);
        }
        if let Some(id) = sample {
            self.open_sample(id);
        }
    }

    /// The name box: opening it, and going where it says.
    ///
    /// `Ctrl+P`, which is what an editor has taught everybody the gesture is.
    /// Also a button, because a feature reachable only by a shortcut is a
    /// feature most people never find.
    fn take_palette(&mut self, ui: &Ui) {
        // Not while something else has the keyboard — a `P` typed into the find
        // box is a `P`, and the modifier is not enough on its own to tell them
        // apart in a window with text fields in it.
        let asked = ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::P));
        if asked && self.open.is_some() && ui.memory(|memory| memory.focused().is_none()) {
            self.palette.show();
        }
        if !self.palette.open {
            return;
        }
        let Some(mut open) = self.open.take() else {
            self.palette.open = false;
            return;
        };
        let chosen = crate::palette::show(ui, &mut self.palette, &open.design, &open.flat);
        match chosen {
            Some(crate::palette::Found::Module(module)) => {
                open.goto(module);
                self.status = format!("`{}`", open.design.module(module).name);
            }
            // The same as clicking its wire in the diagram: lit, shown in the
            // source, and put at the head of the trail. One way of arriving
            // rather than a second kind of selection.
            Some(crate::palette::Found::Signal(signal)) => {
                if let Some((_, net)) = home_of_signal(&open, signal) {
                    self.pick_net(&mut open, net);
                }
            }
            None => {}
        }
        self.open = Some(open);
    }

    fn open_in_editor(&mut self, files: &FileTable, span: Span) {
        let Some(path) = files.path(span.file) else {
            self.status = "that element has no source file".to_string();
            return;
        };

        // Split first, substitute after. The other way round, a source path
        // holding a space — `C:\Program Files\...` — was torn into two
        // arguments, and the editor opened a file by the first half of its name.
        let mut parts = split_command(&self.editor_command).into_iter().map(|part| {
            part.replace("{file}", &path.display().to_string())
                .replace("{line}", &span.line.to_string())
                .replace("{col}", &span.col.to_string())
        });
        let Some(program) = parts.next() else {
            self.status = "no editor command configured".to_string();
            return;
        };

        // Resolved the way a shell resolves it, not the way `CreateProcess`
        // does: VS Code on the `PATH` is `code.cmd`, and a bare `code` finds
        // nothing at all. Measured — the button did nothing and said "The
        // system cannot find the file specified" in the corner.
        let Some(mut command) = rtlscope_tb::Tools::default().program(&program) else {
            self.status = format!(
                "`{program}` is not on the PATH — start with `--editor '<command> {{file}}'`                  to say how to open a file"
            );
            return;
        };
        match command.args(parts).spawn() {
            Ok(_) => self.status = format!("opened {}", files.render(span)),
            Err(error) => self.status = format!("could not run `{program}`: {error}"),
        }
    }

    /// The file table of whatever is loaded, cloned so a caller may hold it
    /// while it mutates the rest of the application.
    fn files(&self) -> Option<FileTable> {
        self.open.as_ref().map(|open| open.design.files.clone())
    }
}

impl eframe::App for RtlScopeApp {
    /// Takes the note back down on the way out, so nothing claims a window is
    /// open when none is.
    fn on_exit(&mut self) {
        self.save_layout();
        rtlscope_sv::Session::clear();
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        ui.ctx().set_theme(self.theme_preference);
        // `Ctrl+O`, which every program means "open" by. Not while a text
        // field has the keyboard, for the same reason `Ctrl+P` waits.
        if ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::O))
            && ui.memory(|memory| memory.focused().is_none())
        {
            self.browse(ui.ctx(), views::Browse::Files);
        }
        self.collect_picked();
        self.take_palette(ui);
        self.collect_simulation(ui);
        self.watch_sources(ui);
        self.publish_session();
        self.take_dropped(ui);
        // What size the reader has left the main window at. Believable
        // values only: a rect of zero comes back from a minimised window, and
        // writing that down would reopen at nothing.
        if let Some(inner) = ui.ctx().input(|input| input.viewport().inner_rect)
            && inner.width() > 200.0
            && inner.height() > 200.0
        {
            self.main_size = Some([inner.width(), inner.height()]);
        }

        // Told once a frame, before anything that draws the simulate button:
        // it is drawn from two views that know nothing about a testbench, and
        // threading one through both would carry it past everything else they
        // do.
        views::set_bench(ui.ctx(), self.testbench().map(|bench| bench.top.clone()));
        self.toolbar(ui);
        self.status_bar(ui);
        // Last, so the desk gets whatever the two fixed strips left over.
        self.desk(ui);
        // The dock is back in hand, so anything a view asked for while it was
        // out can be done now.
        self.apply_dock_requests();
    }
}

impl RtlScopeApp {
    fn toolbar(&mut self, ui: &mut Ui) {
        // Read after the row is built: the menu borrows `ui`, and acting on
        // what it was asked for needs all of `self`.
        let mut show: Option<Tab> = None;
        let mut wrote = false;
        Panel::top("toolbar").show(ui, |ui| {
            let theme = Theme::of(ui);
            let mut go_back_to: Option<usize> = None;
            let mut refit = false;

            ui.horizontal(|ui| {
                theme::brand_mark(ui, theme);
                ui.label(RichText::new("RTLScope").strong());
                ui.separator();
                self.open_menu(ui);
                ui.separator();

                // A testbench is a second thing this session holds, and what
                // `simulate` does depends on whether one is here. That has to
                // be visible: a button whose meaning changed with something
                // chosen from a menu twenty minutes ago is a button nobody can
                // predict.
                if let Some(name) = self.testbench().map(|bench| bench.top.clone()) {
                    theme::badge(ui, &format!("tb: {name}"), theme.accent, theme.accent_soft);
                    if ui
                        .small_button("×")
                        .on_hover_text(
                            "Put the testbench down; simulating generates a harness again",
                        )
                        .clicked()
                    {
                        self.drop_testbench();
                    }
                    ui.separator();
                }

                match self.open.as_ref() {
                    None => {
                        ui.label(RichText::new("no design open").weak());
                    }
                    Some(open) => {
                        // The drill-down path, each step clickable to go back.
                        for (depth, crumb) in open.path.iter().enumerate() {
                            if depth > 0 {
                                ui.label(RichText::new("›").weak());
                            }
                            // The instance is what a dump knows this by, so it
                            // leads.
                            let shown = open.design.module(crumb.module).shown();
                            let name = match &crumb.instance {
                                Some(instance) => instance.as_str(),
                                None => shown.as_ref(),
                            };
                            let is_last = depth + 1 == open.path.len();
                            let text = if is_last {
                                RichText::new(name).monospace().strong()
                            } else {
                                RichText::new(name).monospace()
                            };
                            if ui.selectable_label(is_last, text).clicked() && !is_last {
                                go_back_to = Some(depth + 1);
                            }
                        }
                    }
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // System, then explicit: the default follows the OS, and
                    // one click pins it.
                    let label = match self.theme_preference {
                        ThemePreference::System => "auto",
                        ThemePreference::Light => "light",
                        ThemePreference::Dark => "dark",
                    };
                    if ui
                        .button(format!("◑ {label}"))
                        .on_hover_text("Theme: follows the system until you pin one")
                        .clicked()
                    {
                        self.theme_preference = match self.theme_preference {
                            ThemePreference::System => ThemePreference::Light,
                            ThemePreference::Light => ThemePreference::Dark,
                            ThemePreference::Dark => ThemePreference::System,
                        };
                        // The same state the Settings tab shows, so the two
                        // cannot disagree and neither is the only way to keep
                        // a choice.
                        wrote = true;
                    }
                    // Between the theme and the settings. The far right of this
                    // row is where the window's own affairs live rather than
                    // the design's, and how to work the window is one of them.
                    self.help_menu(ui);
                    // Beside the reset, because both are about the window
                    // rather than about any design in it.
                    self.settings_menu(ui);
                    if ui
                        .button("reset layout")
                        .on_hover_text("Put every view back where it started, and forget the rest")
                        .clicked()
                    {
                        self.reset_layout();
                    }
                    ui.separator();
                    // Where every view can be reached, including any the reader
                    // has closed. A dock without this is a window that loses a
                    // view for good the first time somebody presses the wrong x.
                    show = self.view_menu(ui);
                    // Next door to `view`, because the two questions are: what
                    // is there to know about this design, and where has the
                    // view that says it gone.
                    if let Some(tab) = self.tools_menu(ui) {
                        show = Some(tab);
                    }
                    if self.open.is_some() {
                        if ui
                            .button("search")
                            .on_hover_text("Find a module or a signal by name  (Ctrl+P)")
                            .clicked()
                        {
                            self.palette.show();
                        }
                        refit = ui.button("fit").on_hover_text("Fit the whole diagram").clicked();
                        ui.checkbox(&mut self.show_clocks, "clocks").on_hover_text(
                            "Clock and reset wires reach every flop; hidden by default so the \
                             structure stays visible.",
                        );
                    }
                });
            });

            if let Some(open) = self.open.as_mut() {
                if let Some(depth) = go_back_to {
                    open.path.truncate(depth);
                    open.selected = None;
                    open.highlighted.clear();
                }
                if refit {
                    // An invalid rect makes Scene refit to the contents.
                    let module = open.current();
                    open.scene_rects.insert(module, Rect::ZERO);
                }
            }
        });
        if wrote {
            self.save_settings();
        }
        if let Some(tab) = show {
            self.show(tab);
        }
    }

    /// Every view this design can be looked at through, and where it is.
    ///
    /// A menu rather than a row of tabs, because the tabs are now on the panes
    /// themselves — the reader arranges them, so a second fixed row naming the
    /// same things would be claiming an authority it no longer has. What is
    /// left is the question the old row also answered: what *can* this tool
    /// show me, and where has that gone. A tick marks what is on screen, grey
    /// marks a view that is not on the desk at all, and either way clicking it
    /// brings it up.
    fn view_menu(&self, ui: &mut Ui) -> Option<Tab> {
        let mut asked: Option<Tab> = None;
        ui.menu_button("view", |ui| {
            for tab in Tab::ALL {
                let title = self.title_of(tab);
                let text = match self.has(tab) {
                    true => RichText::new(title),
                    // Said rather than hidden: a reader who closed a view
                    // should be able to find it without remembering that they
                    // did.
                    false => RichText::new(title).weak(),
                };
                if ui.selectable_label(self.is_visible(tab), text).clicked() {
                    asked = Some(tab);
                    ui.close();
                }
            }
        })
        .response
        .on_hover_text("Every view. One that was closed comes back where it belongs.");
        asked
    }

    /// Every view that says something *about* the design, with the question it
    /// answers.
    ///
    /// Beside `view`, which lists all eleven by name, because a name is not
    /// enough to tell anybody what CDC is for — and the analyses that took the
    /// most work to write were the ones nobody opened. The gesture is `view`'s:
    /// what is named comes to the front wherever it already is, and one that
    /// was closed comes back with the reports.
    fn tools_menu(&self, ui: &mut Ui) -> Option<Tab> {
        let theme = Theme::of(ui);
        let mut asked: Option<Tab> = None;
        ui.menu_button("tools", |ui| {
            // Wide enough for the sentences rather than for the names: a line
            // that wraps in the middle of what a tool is for defeats the point
            // of writing it down.
            ui.set_min_width(500.0);
            if self.open.is_none() {
                ui.label(
                    RichText::new("No design open. Every tool below will say so until there is.")
                        .small()
                        .weak(),
                );
                ui.separator();
            }
            for tab in Tab::TOOLS {
                let on_screen = self.is_visible(tab);
                ui.horizontal(|ui| {
                    let name = egui::Button::selectable(on_screen, self.title_of(tab));
                    if ui.add_sized([116.0, 22.0], name).clicked() {
                        asked = Some(tab);
                        ui.close();
                    }
                    // What is on screen reads in full ink and the rest grey, so
                    // the list says where you are as well as where you could
                    // go — without a column of ticks to read past.
                    let text = RichText::new(what_it_answers(tab)).small();
                    ui.label(match on_screen {
                        true => text.color(theme.ink),
                        false => text.weak(),
                    });
                });
            }
        })
        .response
        .on_hover_text("What there is to know about a design, and which view says it.");
        asked
    }

    /// What the window will not say by being looked at.
    ///
    /// Beside `settings`, because both are about the window rather than about
    /// any design in it. `view` next door already says what each view answers,
    /// so what is left for this is the part nobody finds by pointing: a
    /// double-click that descends into a block leaves no mark saying it will,
    /// and a key that deliberately does nothing while a text field has the
    /// keyboard does nothing in the one place a reader tries it first.
    ///
    /// Every line here was read off the code that implements it rather than
    /// off memory of what it ought to do. A help that is wrong is worse than
    /// no help, because it is believed.
    fn help_menu(&self, ui: &mut Ui) {
        ui.menu_button("help", |ui| {
            // The width `settings` uses, for the reason `settings` uses it.
            ui.set_min_width(500.0);
            ui.set_max_width(500.0);

            ui.label(RichText::new("Keys").strong());
            ui.label(
                RichText::new(
                    "None of these fire while a text field has the keyboard: a `P` typed \
                     into the find box is a `P`.",
                )
                .small()
                .weak(),
            );
            help_rows(ui, "help-keys", true, &HELP_KEYS);

            ui.add_space(8.0);
            ui.separator();
            ui.label(RichText::new("In the waveform").strong());
            ui.label(
                RichText::new("While the pointer is over it, and nothing else has the keyboard.")
                    .small()
                    .weak(),
            );
            help_rows(ui, "help-wave", true, &HELP_WAVE);

            ui.add_space(8.0);
            ui.separator();
            ui.label(RichText::new("Gestures").strong());
            ui.label(
                RichText::new(
                    "One click selects, and it is the second that goes somewhere. Nothing \
                     here writes to your source.",
                )
                .small()
                .weak(),
            );
            help_rows(ui, "help-gestures", false, &HELP_GESTURES);

            ui.add_space(8.0);
            ui.separator();
            let note = match crate::layout::settings_dir() {
                Some(dir) => format!(
                    "RTLScope {} · what this window remembers is in {}",
                    env!("CARGO_PKG_VERSION"),
                    dir.display()
                ),
                None => format!(
                    "RTLScope {} · this machine names no place to keep what the window \
                     remembers, so it lasts as long as the window does.",
                    env!("CARGO_PKG_VERSION")
                ),
            };
            // Truncated rather than wrapped, for the reason the same note under
            // `settings` is: a path is one thing, and a menu that grows to the
            // length of somebody's home directory is not.
            ui.add(egui::Label::new(RichText::new(note).small().weak()).truncate());
        });
    }

    /// The ground everything is drawn on, and the tool that runs a simulation.
    ///
    /// Beside `reset layout`, because both are about the window rather than
    /// about any design in it. Each change is written to disk the moment it is
    /// made rather than at exit, so a window that is killed rather than closed
    /// does not forget what it was told.
    fn settings_menu(&mut self, ui: &mut Ui) {
        let theme = Theme::of(ui);
        // Off the filesystem, so once rather than once a frame; see `sim_found`.
        let engines: Vec<(rtlscope_tb::Engine, bool)> =
            ENGINES.iter().map(|engine| (*engine, self.engine_found(*engine))).collect();
        let (chosen, preference) = (self.engine, self.theme_preference);
        let where_written = crate::layout::Settings::path();
        let (mut wanted_theme, mut wanted_engine) = (preference, chosen);
        // Edited as copies and put back afterwards, the way the two choices
        // above are: a menu closure that held the application would leave
        // nothing to compare against to know something was changed.
        let (was_sim_dir, was_python) = (self.sim_dir.clone(), self.python.clone());
        let (mut sim_dir, mut python) = (was_sim_dir.clone(), was_python.clone());
        let was_editor = self.editor_command.clone();
        let mut editor = was_editor.clone();
        let pinned = self.editor_pinned;
        // One `stat` on what was typed, rather than the walk `Tools::python`
        // does: this box is about the path in it, and the walk is what happens
        // when it is empty.
        let python_there = filled_in(&python).is_some_and(|path| path.is_file());

        ui.menu_button("settings", |ui| {
            ui.set_min_width(500.0);
            // And no wider. A menu sizes itself to its widest child, so one
            // unwrapped sentence — or a text box that asked for all the room
            // there was — took it to twice this and left the three sections
            // above it lost in a field of white. Measured, at 990 of 1080.
            ui.set_max_width(500.0);
            ui.label(RichText::new("Theme").strong());
            ui.label(
                RichText::new("`auto` follows the operating system; the others pin one.")
                    .small()
                    .weak(),
            );
            ui.horizontal(|ui| {
                for (option, label) in [
                    (ThemePreference::System, "auto"),
                    (ThemePreference::Light, "light"),
                    (ThemePreference::Dark, "dark"),
                ] {
                    // Framed even when it is not the one chosen: these are a
                    // choice of two or three, and an option that does not look
                    // pressable is one nobody presses.
                    let button = egui::Button::selectable(preference == option, label)
                        .frame_when_inactive(true);
                    if ui.add_sized([64.0, 22.0], button).clicked() {
                        wanted_theme = option;
                    }
                }
            });

            ui.add_space(8.0);
            ui.separator();
            ui.label(RichText::new("Simulator").strong());
            ui.label(
                RichText::new(
                    "What `simulate this design` runs. There is no fallback between them: \
                     the one chosen here is what runs, or the reason nothing did.",
                )
                .small()
                .weak(),
            );
            for (engine, found) in &engines {
                ui.horizontal(|ui| {
                    let button = egui::Button::selectable(chosen == *engine, engine.name())
                        .frame_when_inactive(true);
                    if ui.add_sized([116.0, 22.0], button).clicked() {
                        wanted_engine = *engine;
                    }
                    ui.label(RichText::new(what_it_is(*engine)).small().weak());
                });
                ui.horizontal(|ui| {
                    // Under the name it is about, indented past the button.
                    ui.add_space(124.0);
                    // Not "on the PATH" any more: the directory named below is
                    // looked in first, so a tool can be found without one.
                    let (mark, colour) = match found {
                        true => ("found", theme.ok),
                        false => ("not found", theme.warn),
                    };
                    ui.label(RichText::new(mark).small().color(colour));
                    if !found {
                        let how = format!("· {}", engine.how_to_get_it());
                        ui.label(RichText::new(how).small().monospace().weak());
                    }
                });
            }

            ui.add_space(8.0);
            ui.separator();
            ui.label(RichText::new("Where the tools are").strong());
            ui.label(
                RichText::new(
                    "Only needed when this window cannot find them itself. Opened by \
                     double-clicking a file it inherits no shell's PATH and stands nowhere \
                     near a checkout — so these are empty for exactly the people who need \
                     them. Both go ahead of the search, never instead of it.",
                )
                .small()
                .weak(),
            );

            // The theme asks an input to read as raised against the sheet it
            // sits on, and a menu's sheet is the same white a text box is given
            // by default — so both boxes came out as bare hint text with no
            // edge, which is not something anybody clicks into.
            ui.visuals_mut().extreme_bg_color = theme.surface_alt;

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_sized([116.0, 22.0], egui::Label::new(RichText::new("simulator")));
                ui.add(
                    egui::TextEdit::singleline(&mut sim_dir)
                        .hint_text("the directory holding it")
                        .desired_width(360.0),
                );
            });
            ui.horizontal(|ui| {
                ui.add_space(124.0);
                // Wrapped explicitly: a label inside a horizontal layout is
                // given as much room as it asks for, and a sentence asks for
                // all of it. That is what took this menu to twice its width.
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "Where `verilator`, or `iverilog` and `vvp`, live. The file \
                             itself is taken too — its directory is what gets used.",
                        )
                        .small()
                        .weak(),
                    )
                    .wrap(),
                );
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_sized([116.0, 22.0], egui::Label::new(RichText::new("python")));
                ui.add(
                    egui::TextEdit::singleline(&mut python)
                        .hint_text("a python.exe with cocotb in it")
                        .desired_width(360.0),
                );
            });
            ui.horizontal(|ui| {
                ui.add_space(124.0);
                // Said about what was typed, and only when something was: an
                // empty box is not wrong, it is the ordinary case.
                if !python.trim().is_empty() {
                    let (mark, colour) = match python_there {
                        true => ("there", theme.ok),
                        false => ("not there", theme.warn),
                    };
                    ui.label(RichText::new(mark).small().color(colour));
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "What plays a drawn pattern. Left empty, a `.venv-cocotb` in \
                             or above the design is found on its own — this is for when \
                             there is none to find. On Windows, Verilator needs the ucrt64 \
                             Python; Icarus takes any.",
                        )
                        .small()
                        .weak(),
                    )
                    .wrap(),
                );
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_sized([116.0, 22.0], egui::Label::new(RichText::new("editor")));
                ui.add(
                    egui::TextEdit::singleline(&mut editor)
                        .hint_text(DEFAULT_EDITOR)
                        .desired_width(360.0),
                );
            });
            ui.horizontal(|ui| {
                ui.add_space(124.0);
                if pinned {
                    ui.label(RichText::new("--editor wins this run").small().color(theme.warn));
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "What `open in editor` runs. `{file}`, `{line}` and `{col}` are                              filled in; quote a program whose path has a space in it.",
                        )
                        .small()
                        .weak(),
                    )
                    .wrap(),
                );
            });

            ui.add_space(8.0);
            ui.separator();
            let note = match &where_written {
                Some(path) => format!("Kept in {}", path.display()),
                None => "This machine names no place to keep these, so they last as long as \
                         the window does."
                    .to_string(),
            };
            // Truncated rather than wrapped: a path is one thing, and a menu
            // that grows to the length of somebody's home directory is not.
            ui.add(egui::Label::new(RichText::new(note).small().weak()).truncate());
            ui.label(
                RichText::new(
                    "`reset layout` puts the views back and leaves these alone: where your \
                     panes are and what you like are two different things.",
                )
                .small()
                .weak(),
            );
        })
        .response
        .on_hover_text("What this window looks like, and what it simulates with.");

        // Typing in the box is a choice, so it takes the flag's place rather
        // than sitting behind it until the next start.
        if editor != was_editor {
            self.editor_command = editor;
            self.editor_pinned = false;
        }
        let moved =
            sim_dir != was_sim_dir || python != was_python || self.editor_command != was_editor;
        if moved {
            // The one thing that changes where a tool is without the filesystem
            // changing, so the cached answer has to go with it — otherwise the
            // menu keeps saying "not found" about a directory just named.
            self.sim_found.clear();
            self.sim_dir = sim_dir;
            self.python = python;
        }
        if moved || wanted_theme != preference || wanted_engine != chosen {
            self.theme_preference = wanted_theme;
            self.engine = wanted_engine;
            self.save_settings();
        }
    }

    /// Where this window looks for the two things it does not run itself.
    ///
    /// Anchored later, at the design's own files, by whoever is about to
    /// simulate: this side knows only what was typed under `settings`.
    fn tools(&self) -> rtlscope_tb::Tools {
        rtlscope_tb::Tools {
            python: filled_in(&self.python),
            sim_dir: filled_in(&self.sim_dir),
            near: Vec::new(),
        }
    }

    /// Whether a simulator's tools can be found, asked once per run.
    fn engine_found(&mut self, engine: rtlscope_tb::Engine) -> bool {
        match self.sim_found.get(engine.name()) {
            Some(found) => *found,
            None => {
                let found = self.tools().available(engine);
                self.sim_found.insert(engine.name(), found);
                found
            }
        }
    }

    /// Puts back what the reader last chose: the ground, and the simulator.
    ///
    /// Read here rather than in `new`, for the same reason the layout is: a
    /// test that builds an application would otherwise pick up the settings of
    /// whoever is running it, and a suite whose result depends on the
    /// developer's colour scheme is not a suite.
    ///
    /// `RTLSCOPE_THEME` wins over the file. That knob exists so a screenshot can
    /// ask for a ground without changing anything to get it, and a saved
    /// preference quietly overruling it would make the knob useless on exactly
    /// the machine it is used on.
    pub fn restore_saved_settings(&mut self) {
        self.take_settings(crate::layout::Settings::read());
    }

    /// The same, from settings already in hand.
    ///
    /// Apart so a test can say what was saved instead of reaching for the
    /// settings of whoever is running it — the reason [`crate::layout`] splits
    /// `read` from `read_from`.
    fn take_settings(&mut self, saved: crate::layout::Settings) {
        if std::env::var("RTLSCOPE_THEME").is_err()
            && let Some(theme) = saved.theme.as_deref()
        {
            self.theme_preference = match theme {
                "light" => ThemePreference::Light,
                "dark" => ThemePreference::Dark,
                // Including a name this build does not know: following the
                // system is the answer that is never wrong.
                _ => ThemePreference::System,
            };
        }
        if let Some(engine) = saved.engine.as_deref() {
            self.engine = match engine {
                "icarus" => rtlscope_tb::Engine::Icarus,
                _ => rtlscope_tb::Engine::Verilator,
            };
        }
        // Not checked for existing here. A path that has gone should be said
        // by name when something needs it, not silently dropped on the way in
        // and then reported as though it was never typed.
        self.sim_dir = saved.sim_dir.unwrap_or_default();
        self.python = saved.python.unwrap_or_default();
        if !self.editor_pinned
            && let Some(editor) = saved.editor
        {
            self.editor_command = editor;
        }
    }

    /// Writes down what was just chosen.
    fn save_settings(&self) {
        let theme = match self.theme_preference {
            ThemePreference::System => "auto",
            ThemePreference::Light => "light",
            ThemePreference::Dark => "dark",
        };
        let settings = crate::layout::Settings {
            theme: Some(theme.to_string()),
            engine: Some(self.engine.name().to_string()),
            // Emptied rather than written blank: an absent key means "look the
            // old way", and a `""` in the file would be a path to nowhere.
            sim_dir: filled_in(&self.sim_dir).map(|path| path.display().to_string()),
            python: filled_in(&self.python).map(|path| path.display().to_string()),
            // A `--editor` given for this run must not overwrite the choice
            // the file holds, so while one is pinned the file keeps its own.
            editor: match self.editor_pinned {
                true => crate::layout::Settings::read().editor,
                false => Some(self.editor_command.clone()),
            },
        };
        let _ = settings.write();
    }

    /// The module tree, which used to be the fixed panel down the left.
    fn hierarchy_tab(&mut self, ui: &mut Ui) {
        let Some(open) = self.open.as_mut() else {
            views::empty_state(
                ui,
                "no design open",
                "Drop sources on the window, or open some from the toolbar.",
            );
            return;
        };
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{} module(s)", open.design.modules.len()))
                        .small()
                        .weak(),
                );
            });
        });
        ui.separator();
        let mut top: Option<String> = None;
        ScrollArea::vertical().id_salt("hierarchy").show(ui, |ui| {
            let current = open.current();
            let asked = tree::show(ui, &open.design, current);
            if let Some(module) = asked.open {
                // Jumping through the tree replaces the path rather than
                // extending it, since the tree is absolute, not relative.
                open.goto(module);
            }
            if let Some(module) = asked.top {
                // By the name in the source: the pin is what `--top` takes,
                // and a specialisation suffix is not something a reader typed.
                top = Some(open.design.module(module).base_name.clone());
            }
        });
        // ---- the testbench, when one is in hand ----
        //
        // Its own heading rather than a row among the design's modules: a
        // testbench is not part of the design, and drawn among its modules it
        // would read as one more of them — which is the mistake `open_testbench`
        // exists to avoid. Under it, the instance that joins the two trees is
        // the way across.
        let mut read_bench: Option<Span> = None;
        let mut goto: Option<ModuleId> = None;
        if let Some(bench) = self.bench.as_ref() {
            ui.separator();
            ui.label(RichText::new("simulation source").small().weak());
            let file = bench
                .table
                .path(bench.span.file)
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            if ui
                .selectable_label(open.source_in_bench, RichText::new(&bench.top).monospace())
                .on_hover_text(format!("{file}\nclick to read it"))
                .clicked()
            {
                read_bench = Some(bench.span);
            }
            for (instance, module) in &bench.instances {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    let text = RichText::new(format!("{instance} : {module}")).monospace();
                    // By the name written, then by the name the design gives a
                    // specialisation of it: `cpu` is instantiated as `cpu`,
                    // and a parameterised module under a suffix.
                    let found =
                        open.design.module_by_name(module).map(|(id, _)| id).or_else(|| {
                            open.design
                                .modules
                                .iter_enumerated()
                                .find(|(_, it)| it.base_name == *module)
                                .map(|(id, _)| id)
                        });
                    match found {
                        Some(id) => {
                            if ui
                                .selectable_label(false, text)
                                .on_hover_text("The design, under test. Click to look at it.")
                                .clicked()
                            {
                                goto = Some(id);
                            }
                        }
                        None => {
                            ui.label(text.weak()).on_hover_text("Not a module of this design.");
                        }
                    }
                });
            }
            if let Some(note) = bench.notes.first() {
                ui.label(RichText::new(note).small().weak());
            }
        }
        if let Some(span) = read_bench {
            open.show_bench_source(span);
        }
        if let Some(id) = goto {
            open.goto(id);
        }
        if let Some(name) = top {
            self.set_top(name);
        }
        if read_bench.is_some() {
            self.show(Tab::Source);
        }
    }

    /// Reads the open design's sources again with one module as the top.
    ///
    /// The same thing [`Self::read_as_top`] does for a read that failed, for a
    /// design that is open: the sources it was read from are known, so there
    /// is nothing to ask for. The choice sticks the way `--top` does.
    fn set_top(&mut self, top: String) {
        let Some(paths) = self.open.as_ref().map(|open| open.source_paths.clone()) else {
            return;
        };
        self.top = Some(top);
        self.load(&paths, false);
    }

    /// One thin line that is always true: what is selected, and what happened
    /// last.
    fn status_bar(&mut self, ui: &mut Ui) {
        Panel::bottom("statusbar").resizable(false).min_size(24.0).max_size(24.0).show(ui, |ui| {
            let theme = Theme::of(ui);
            let mut open_span: Option<Span> = None;
            let mut show_span: Option<Span> = None;
            let mut show_diagnostics = false;

            ui.horizontal(|ui| {
                let selected = self.open.as_ref().and_then(|open| {
                    open.selected.as_ref().map(|selected| {
                        (
                            selected.label.clone(),
                            open.design.files.render(selected.span),
                            selected.span,
                        )
                    })
                });
                match selected {
                    Some((label, location, span)) => {
                        ui.label(RichText::new(label).monospace().strong());
                        ui.label(RichText::new(location).monospace().small().weak());
                        if ui
                            .link(RichText::new("source").small())
                            .on_hover_text("Show this line in the Source tab")
                            .clicked()
                        {
                            show_span = Some(span);
                        }
                        if ui
                            .link(RichText::new("editor").small())
                            .on_hover_text("Open this line in the external editor")
                            .clicked()
                        {
                            open_span = Some(span);
                        }
                    }
                    None if self.open.is_some() => {
                        ui.label(
                            RichText::new(
                                "click a box to select · double-click to enter · click a wire \
                                 to trace it",
                            )
                            .small()
                            .weak(),
                        );
                    }
                    None => {
                        ui.label(
                            RichText::new("drop sources on the window to begin").small().weak(),
                        );
                    }
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(open) = self.open.as_ref() {
                        let errors = open.diags.count(Severity::Error);
                        let warnings = open.diags.count(Severity::Warning);
                        show_diagnostics =
                            theme::badge(ui, &format!("{warnings}W"), theme.warn, theme.warn_soft)
                                .on_hover_text("Warnings — click for the list")
                                .clicked()
                                || theme::badge(
                                    ui,
                                    &format!("{errors}E"),
                                    theme.err,
                                    theme.err_soft,
                                )
                                .on_hover_text("Errors — click for the list")
                                .clicked();
                    }
                    if !self.status.is_empty() {
                        ui.separator();
                        ui.label(RichText::new(&self.status).small().weak());
                    }
                });
            });

            if show_diagnostics {
                self.show(Tab::Diagnostics);
            }
            if let Some(span) = open_span
                && let Some(files) = self.files()
            {
                self.open_in_editor(&files, span);
            }
            if let Some(span) = show_span
                && let Some(open) = self.open.as_mut()
            {
                open.show_source(span);
                self.show(Tab::Source);
            }
        });
    }

    /// The desk, drawn: every view, wherever the reader has put it.
    ///
    /// The dock is taken out of the application for the length of this call and
    /// put back at the end, the same trick [`Open`] gets in `view`. `DockArea`
    /// borrows it, and the views it then draws have to be able to touch the
    /// whole application — a click in the diagram moves the source, a button in
    /// the waveform starts a simulation. Nothing observes it missing: the two
    /// things that would care, `show` and `detach`, write down what they were
    /// asked for instead, and it is done as soon as this returns.
    fn desk(&mut self, ui: &mut Ui) {
        let theme = Theme::of(ui);
        // An empty window keeps the drop target it always had, whole. Tab
        // strips over an empty hole would be a tool advertising nine views it
        // cannot draw, to somebody who has not yet given it a design.
        if self.open.is_none() && self.wave.is_none() {
            let frame = egui::Frame::new().fill(theme.canvas_bg);
            CentralPanel::default().frame(frame).show(ui, |ui| self.welcome(ui));
            return;
        }

        let Some(mut dock) = self.dock.take() else { return };
        self.visible = dock::visible(&dock);
        let style = theme::dock_style(ui);
        CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            // The bounds for the floating windows: what is left between the
            // toolbar and the status bar, so a float cannot come to rest on top
            // of either and cannot be dragged off the screen entirely.
            let bounds = ui.max_rect();
            DockArea::new(&mut dock)
                .id(egui::Id::new("rtlscope-dock"))
                .style(style)
                .window_bounds(bounds)
                .show_close_buttons(true)
                .show_add_buttons(false)
                // Both off: closing a whole group at once, or folding one away
                // to a strip, are gestures whose undo is "find it in the view
                // menu again", and the tab bars here are short enough that
                // neither buys anything.
                .show_leaf_close_all_buttons(false)
                .show_leaf_collapse_buttons(false)
                .show_inside(ui, &mut Viewer { app: self, theme });
        });
        // After the drawing, when egui knows where each window ended up — a
        // drag this frame included.
        self.floats = dock::float_places(&dock, ui.ctx());
        self.dock = Some(dock);
    }

    /// Puts a view where the reader can see it.
    ///
    /// Every "go to the waveform", "show me that line" and "open the Trace
    /// view" in this file comes here, so that they cannot come to disagree
    /// about what showing a view means. If the dock is out — this was called
    /// from inside a view being drawn — the move is written down and made the
    /// moment the frame's drawing is over.
    pub(crate) fn show(&mut self, tab: Tab) {
        match self.dock.as_mut() {
            Some(dock) => dock::show(dock, tab),
            None => self.dock_requests.push(DockRequest::Show(tab)),
        }
    }

    /// Gives a view a window of its own, floating over the rest.
    pub(crate) fn detach(&mut self, tab: Tab) {
        let main = self.main_size;
        match self.dock.as_mut() {
            Some(dock) => dock::detach(dock, tab, main),
            None => self.dock_requests.push(DockRequest::Detach(tab)),
        }
    }

    /// Whether the desk holds this view at all, in front or behind another tab.
    fn has(&self, tab: Tab) -> bool {
        self.dock.as_ref().is_some_and(|dock| dock::has(dock, tab))
    }

    /// Whether this view is on screen.
    ///
    /// From the dock when there is one, and from the frame's own snapshot when
    /// there is not — which is the case whenever a view asks this while it is
    /// itself being drawn. Reading `None` as "nothing is visible" would make
    /// every location clicked in a view fail to move the source, quietly.
    fn is_visible(&self, tab: Tab) -> bool {
        match self.dock.as_ref() {
            Some(dock) => dock::is_visible(dock, tab),
            None => self.visible.contains(&tab),
        }
    }

    /// The view in the group the reader last touched.
    fn focused_tab(&self) -> Option<Tab> {
        self.dock.as_ref().and_then(dock::focused)
    }

    /// Does whatever was asked for while the dock was out of reach.
    fn apply_dock_requests(&mut self) {
        for request in std::mem::take(&mut self.dock_requests) {
            match request {
                DockRequest::Show(tab) => self.show(tab),
                DockRequest::Detach(tab) => self.detach(tab),
            }
        }
    }

    /// Reads the saved arrangement, once.
    pub fn restore_saved_layout(&mut self) {
        self.restore_layout();
    }

    /// Puts the desk back the way it was left.
    ///
    /// Anything the file names that is no longer a view is dropped without
    /// comment, and a file that describes nothing this build can make leaves
    /// the default arrangement standing. A layout is a convenience, and a
    /// convenience that complains is worse than one that quietly does its best.
    fn restore_layout(&mut self) {
        let saved = crate::layout::Layout::read();
        let Some(dock) = saved.dock.and_then(dock::from_saved) else { return };
        self.dock = Some(dock);
        // Replaced rather than added to, so the arrangement is exactly the one
        // that was written down — except for the waveform, which by the time
        // this runs may already have been opened by a dump named on the command
        // line. A file that says nothing about it must not be read as saying it
        // should be shut.
        if self.wave.is_some() && !self.has(Tab::Wave) {
            self.show(Tab::Wave);
        }
    }

    /// Writes the desk down, so the next run opens like this one.
    fn save_layout(&self) {
        let layout = crate::layout::Layout {
            dock: self.dock.as_ref().and_then(|dock| dock::to_saved(dock, &self.floats)),
            main_size: self.main_size,
        };
        let _ = layout.write();
    }

    /// Back to the arrangement a first run sees.
    pub fn reset_layout(&mut self) {
        self.forget_layout();
        crate::layout::Layout::clear();
        self.status = "layout reset".to_string();
    }

    /// The half of a reset that is not the file.
    ///
    /// Apart so a test can check it without deleting the settings of whoever is
    /// running the test.
    fn forget_layout(&mut self) {
        self.dock = Some(dock::default_layout());
        self.dock_requests.clear();
        // A recording that is open is still open, and a desk with no way to
        // look at it would be a reset that lost something.
        if self.wave.is_some() {
            self.show(Tab::Wave);
        }
    }

    /// One view, drawn wherever it is asked for.
    ///
    /// The same function fills a pane in the main window and fills a floating
    /// one, so a view cannot look or behave differently depending on which it
    /// is in — and moving one between them needs no second implementation to
    /// keep in step. This is what the dock draws through; see [`Viewer`].
    fn view(&mut self, ui: &mut Ui, tab: Tab) {
        // The two that used to be fixed places rather than tabs. First, because
        // neither wants the analyses the reports below reach for.
        if tab == Tab::Hierarchy {
            self.hierarchy_tab(ui);
            return;
        }
        if tab == Tab::Diagram {
            self.diagram_tab(ui);
            return;
        }
        if tab == Tab::Wave {
            self.wave_tab(ui);
            return;
        }
        if tab == Tab::Stim {
            self.stim_tab(ui);
            return;
        }

        // Taken out and put back, so a view's action can touch the whole
        // application while the design is being read from.
        let running = self.simulating().map(str::to_string);
        let refused = self.sim_refused().map(str::to_string);
        // Said rather than left blank. A pane is a place the reader put a view
        // on purpose, and one that draws nothing at all reads as broken rather
        // than as waiting.
        let Some(mut open) = self.open.take() else {
            views::empty_state(
                ui,
                "no design open",
                "Drop sources on the window, or open some from the toolbar.",
            );
            return;
        };

        // Source is text, not analysis: reaching for the four reports to draw a
        // file would put a surprising cost on the one tab that does not need
        // them.
        let mut cone_action = crate::cone::ConeAction::default();
        // An editor asked for from the testbench's page. Its span is into the
        // bench's table, and the dispatch below resolves editor spans against
        // the design's — so it is handled here, where the table is known.
        let mut editor_in_bench: Option<Span> = None;
        let action = if tab == Tab::Source {
            let span = open.source_span();
            let bench = self.bench.as_ref().filter(|_| open.source_in_bench);
            if let (Some(bench), Some(_)) = (bench, open.source_target) {
                // The testbench's own table and cache. No names are offered
                // for tracing: its signals are its own, and the design has no
                // wire to light for them.
                let lines = self.bench_sources.lines(&bench.table, span.file);
                let none = BTreeSet::new();
                match views::source(
                    ui,
                    &bench.table,
                    span,
                    lines,
                    &mut open.source_scroll,
                    &none,
                    &[],
                ) {
                    Some(ViewAction::OpenEditor(span)) => {
                        editor_in_bench = Some(span);
                        None
                    }
                    other => other,
                }
            } else {
                let nets = Self::nets_in_view(&open);
                // Disjoint fields of one struct: the table is read while the
                // cache and the scroll flag are written.
                let (files, generated, sources, scroll) = (
                    &open.design.files,
                    &open.design.generated,
                    &mut open.sources,
                    &mut open.source_scroll,
                );
                let lines = sources.lines(files, span.file);
                views::source(ui, files, span, lines, scroll, &nets, generated)
            }
        } else if tab == Tab::Trace {
            // Out here with Source rather than in with the reports: provenance
            // is a query about one net, and making someone in the middle of a
            // hunt pay for four analyses to ask where a wire comes from would
            // be a surprising cost.
            let asked = views::cone_controls(ui, &mut open.cone);
            if asked {
                // A different question is a different picture: fit it rather
                // than leaving the old placement over a cone of another shape.
                open.cone.placement.refit();
            }
            match open.cone.drawn {
                false => views::trace(ui, &open.design, &open.flat, &open.trail),
                true => {
                    let root = open.trail.last().and_then(|hop| signal_of_hop(&open.flat, hop));
                    match root {
                        None => {
                            crate::views::empty_state(
                                ui,
                                "nothing traced yet",
                                "Click a wire in the diagram, or a name in the source, to ask \
                                 what reaches it.",
                            );
                            None
                        }
                        Some(root) => {
                            open.ensure_cone_graph();
                            let graph = open.cone_graph.as_ref().expect("just ensured");
                            let cone = match open.cone.towards {
                                rtlscope_analyse::cone::Towards::Drivers => {
                                    rtlscope_analyse::cone::fan_in(graph, root, open.cone.depth)
                                }
                                rtlscope_analyse::cone::Towards::Loads => {
                                    rtlscope_analyse::cone::fan_out(graph, root, open.cone.depth)
                                }
                            };
                            let mut placement = open.cone.placement;
                            let did = crate::cone::show(
                                ui,
                                &cone,
                                &open.flat,
                                &mut placement,
                                Some(root),
                            );
                            open.cone.placement = placement;
                            cone_action = did;
                            None
                        }
                    }
                }
            }
        } else {
            open.ensure_analyses();
            open.ensure_fsms();
            let analyses = open.analyses.as_ref().expect("just ensured");
            // The field rather than `state_machines()`, which borrows all of
            // `open` and so could not sit beside the `&mut open.fsm` below.
            let machines = open.fsms.as_deref().unwrap_or_default();
            let files = &open.design.files;
            match tab {
                Tab::Diagnostics => match &open.stale {
                    Some(stale) => {
                        views::stale_note(ui, stale.from_add);
                        views::diagnostics(ui, &stale.files, &stale.diags)
                    }
                    None => views::diagnostics(ui, files, &open.diags),
                },
                Tab::Fsm => {
                    let now = machines
                        .get(open.fsm.selected)
                        .and_then(|fsm| now_for(fsm, &open.flat, &open.path, self.wave.as_mut()));
                    views::fsm(ui, files, machines, &mut open.fsm, now)
                }
                Tab::Cdc => views::cdc(ui, files, &analyses.cdc),
                Tab::Lint => views::lint(ui, files, &analyses.lint),
                Tab::Pipeline => {
                    // Whatever the wave panel already worked out, if anything:
                    // both readings colour themselves from that rather than
                    // reading the dump a second time.
                    let occupancy = self
                        .wave
                        .as_ref()
                        .and_then(|wave| wave.stage_cells_at_cursor())
                        .map(|(clock, cycle, cells)| views::Occupancy { clock, cycle, cells });
                    let (flow, cycles, open_wave) = match self.wave.as_mut() {
                        Some(wave) => {
                            let cycles = wave.stage_cycles();
                            (wave.token_flow(), cycles, true)
                        }
                        None => (None, 0, false),
                    };
                    views::pipeline(
                        ui,
                        &analyses.pipeline,
                        &mut open.pipeline_selected,
                        &mut open.pipeline_mode,
                        open_wave,
                        occupancy.as_ref(),
                        flow,
                        cycles,
                        running.as_deref(),
                        refused.as_deref(),
                        &mut self.sim_cycles,
                        SIM_CYCLES_RANGE,
                        &mut open.depth,
                        files,
                    )
                }
                Tab::Hierarchy | Tab::Diagram | Tab::Wave => None,
                Tab::Source | Tab::Trace | Tab::Stim => None,
            }
        };
        // A cone's node names a signal, and a signal has a net in whichever
        // module the reader is standing in. Rerooting first, because a
        // double-click also lands a click and the new question is the one meant.
        if let Some(signal) = cone_action.rerooted.or(cone_action.picked)
            && let Some((_, net)) = home_of_signal(&open, signal)
        {
            self.pick_net(&mut open, net);
            if cone_action.rerooted.is_some() {
                open.cone.placement.refit();
            }
        }
        self.open = Some(open);
        if let Some(span) = editor_in_bench
            && let Some(table) = self.bench.as_ref().map(|bench| bench.table.clone())
        {
            self.open_in_editor(&table, span);
        }
        if let Some(action) = action {
            self.act(action, tab);
        }
    }

    /// What a view is called, and what its name has to say today.
    ///
    /// Two of them carry news as well as a name — which recording is open, and
    /// whether the sources have moved on under the design — because a tab strip
    /// is where the reader is already looking.
    fn title_of(&self, tab: Tab) -> String {
        match tab {
            Tab::Wave => match self.wave.as_ref() {
                Some(state) => format!("Wave · {}", file_name(&state.path)),
                None => "Wave".to_string(),
            },
            Tab::Diagnostics if self.open.as_ref().is_some_and(|open| open.stale.is_some()) => {
                "Diagnostics •".to_string()
            }
            other => other.key().to_string(),
        }
    }

    /// What a view asked for, done.
    ///
    /// `from` is the view that asked, which matters for the one action that has
    /// to come back afterwards: a simulation returns to whichever view pressed
    /// the button.
    fn act(&mut self, action: ViewAction, from: Tab) {
        match action {
            ViewAction::ShowSource(span) => {
                if let Some(open) = self.open.as_mut() {
                    open.show_source(span);
                    self.show(Tab::Source);
                }
            }
            // Softer than showing: the source moves only where the reader can
            // already see it, and never takes the view they are on away.
            ViewAction::PointAt(span) => {
                if self.is_visible(Tab::Source)
                    && let Some(open) = self.open.as_mut()
                {
                    open.show_source(span);
                }
            }
            ViewAction::OpenEditor(span) => {
                if let Some(files) = self.files() {
                    self.open_in_editor(&files, span);
                }
            }
            ViewAction::PickNamed(name) => self.pick_named(&name),
            ViewAction::MeasureDepth => self.measure_depth_pane(),
            ViewAction::Goto(module) => {
                if let Some(open) = self.open.as_mut() {
                    open.goto(module);
                }
            }
            ViewAction::SeekCycle(cycle) => {
                // The same gesture as clicking that moment in the waveform, so
                // every view that follows the cursor moves together.
                let Some(wave) = self.wave.as_mut() else {
                    return;
                };
                match wave.cycle_time(cycle) {
                    Some(at) => {
                        wave.cursor = Some(at);
                        self.status = format!("cycle {cycle}");
                    }
                    None => self.status = format!("cycle {cycle} is not in this dump"),
                }
            }
            ViewAction::WatchState => self.watch_state(),
            ViewAction::SeekNextEntry => self.seek_next_entry(),
            ViewAction::Trace(step) => self.trace_step(step),
            ViewAction::Simulate => self.start_simulation(from),
            ViewAction::FlowWindow(first) => {
                if let Some(wave) = self.wave.as_mut() {
                    wave.build_stage_window(first);
                    self.status = format!("following from cycle {first}");
                }
            }
            ViewAction::OpenStages(clock) => {
                let domain = self.open.as_ref().and_then(|open| {
                    let analyses = open.analyses.as_ref()?;
                    analyses.pipeline.domains.iter().find(|d| d.clock == clock).cloned()
                });
                if let Some(domain) = domain
                    && let Some(wave) = self.wave.as_mut()
                {
                    wave.open_stages(domain);
                    self.status = wave.status.clone();
                    self.show(Tab::Wave);
                }
            }
        }
    }

    /// The wave tab's content, and everything its toolbar asked for.
    fn wave_tab(&mut self, ui: &mut Ui) {
        let running = self.simulating().map(str::to_string);
        let refused = self.sim_refused().map(str::to_string);
        if self.wave.is_none() {
            if views::wave_empty_state(
                ui,
                running.as_deref(),
                refused.as_deref(),
                &mut self.sim_cycles,
                SIM_CYCLES_RANGE,
            )
            .is_some()
            {
                self.start_simulation(Tab::Wave);
            }
            return;
        }

        // Where the reader is standing in the design — `None` for a recording
        // opened on its own. Everything the panel draws comes out of the dump,
        // so it draws either way; the two buttons that read the design say so
        // instead of appearing to do nothing.
        let here = self.open.as_ref().map(|open| (open.current(), tree::instance_path(&open.path)));

        let state = self.wave.as_mut().expect("just checked");
        let action = crate::wave::show(ui, state);
        if action.close {
            self.wave = None;
            self.status = "dump closed".to_string();
            return;
        }
        if action.find_buses {
            match (self.open.as_ref(), here.as_ref()) {
                (Some(open), Some((module, instance))) => {
                    state.buses = state.buses(&open.design, *module, instance);
                    state.status = if state.buses.is_empty() {
                        "no bus was recognised on this module — its signals may be named                          another way"
                            .to_string()
                    } else {
                        format!("{} bus(es) found; pick one to decode", state.buses.len())
                    };
                }
                _ => state.status = needs_a_design("recognising a bus"),
            }
        }
        if let Some(index) = action.decode
            && let Some(suggestion) = state.buses.get(index).cloned()
        {
            state.decode(&suggestion.protocol, &suggestion.bindings);
        }
        if action.find_stages {
            match self.open.as_ref() {
                Some(open) => lay_out_stages(&open.design, state),
                None => state.status = needs_a_design("laying out the pipeline"),
            }
        }
        if action.want_reference {
            // Armed for a drop as well, so cancelling the dialog leaves the
            // gesture half done rather than undone: the recording can still
            // be dragged in.
            self.awaiting_reference = true;
            self.status = "pick the recording to compare against — or drop it in, or start \
                           with --reference"
                .to_string();
            self.browse(ui.ctx(), views::Browse::Waveform);
        }

        // These three leave the panel for the design, so they come after
        // everything that needed it borrowed for drawing. Only one can be
        // asked for in a frame — they are three separate buttons.
        // Before the buttons, so a press that both moves the selection and
        // asks to be taken somewhere ends up where the button said.
        if let Some(signal) = action.followed {
            self.follow_source(signal);
        }
        match (action.show_net, action.show_source, action.trace) {
            (Some(signal), _, _) => self.show_signal(signal, Reveal::Diagram),
            (_, Some(signal), _) => self.show_signal(signal, Reveal::Source),
            (_, _, Some(signal)) => self.show_signal(signal, Reveal::Trace),
            _ => {}
        }
    }

    /// The module the source view is looking at.
    ///
    /// Not the one in the breadcrumb: a reader can be shown a child's source
    /// without the diagram following them there, and the names that mean
    /// something are the ones in the file in front of them.
    ///
    /// Found by file and line, because that is all a span carries. Modules do
    /// not record where they end, so this takes the last one that begins at or
    /// above the line — which is right unless a file nests modules, and
    /// SystemVerilog does not.
    fn module_at(open: &Open, span: Span) -> Option<ModuleId> {
        open.design
            .modules
            .indices()
            .filter(|id| {
                let at = open.design.module(*id).span;
                at.file == span.file && at.line <= span.line
            })
            .max_by_key(|id| open.design.module(*id).span.line)
    }

    /// The names the source view may offer to put on the waveform.
    ///
    /// Only nets of the module on screen. A name that is not one — a keyword
    /// this highlighter does not colour, a parameter, a task — is left plain,
    /// which is the answer to "can I click this" before the click.
    fn nets_in_view(open: &Open) -> BTreeSet<String> {
        let Some(module) = Self::module_at(open, open.source_span()) else {
            return BTreeSet::new();
        };
        open.design
            .module(module)
            .nets
            .iter()
            .filter(|net| !net.synthesised)
            // Both spellings: the file on screen may be the one the author
            // wrote, whose names are the written ones.
            .flat_map(|net| [net.name.clone(), net.shown().to_string()])
            .collect()
    }

    /// A name clicked in the source, treated as its wire in the diagram would
    /// be: traced, lit, and put on the waveform when one is open.
    ///
    /// The name comes from a file, so the instance path has to be worked out:
    /// a dump knows a signal by where it is, and a module instantiated twice
    /// has two of every net. The breadcrumb wins when it is already in that
    /// module, because that is the instance the reader is looking at; failing
    /// that, the first instance of it, and the status says which — silently
    /// picking one of several is how a reader ends up watching the wrong copy.
    ///
    /// Traced quietly, the way a wire click traces. The trail is set and the
    /// Trace view — provenance or cone — answers for it the moment the reader
    /// looks, which with the source beside it they already are; nothing
    /// switches away from the view they clicked from. Unlike a wire click,
    /// the source itself stays put: the reader is at the line they chose, and
    /// jumping it to the declaration would take that away.
    fn pick_named(&mut self, name: &str) {
        let Some(mut open) = self.open.take() else { return };
        let resolved = (|| {
            let module = Self::module_at(&open, open.source_span())?;
            let net = open
                .design
                .module(module)
                .nets
                .indices()
                .find(|id| open.design.module(module).net(*id).name == name)?;
            Some((module, net))
        })();
        let Some((module, net)) = resolved else {
            self.status = format!("`{name}` is not a net of this module");
            self.open = Some(open);
            return;
        };

        // The instance to watch it under.
        let here = open.current();
        let (path, only) = match here == module {
            true => (tree::instance_path(&open.path), true),
            false => {
                let mut of_module =
                    open.flat.nodes.iter().filter(|node| node.module == module).map(|n| &n.path);
                let Some(first) = of_module.next().cloned() else {
                    self.status = format!("`{name}` is in a module the top does not reach");
                    self.open = Some(open);
                    return;
                };
                (first, of_module.next().is_none())
            }
        };

        let crumbs = tree::crumbs_for(&open.design, &path);
        // At the head of the trail, so the Trace view answers for it. A new
        // root is a new picture, so the cone is fitted afresh rather than left
        // at the placement of the last one.
        open.trail = vec![views::Hop { path: crumbs.clone(), net }];
        open.cone.placement.refit();

        let watched = self.watch(&mut open, &crumbs, net);
        // Everything the click did, because dropping any of it leaves the click
        // looking like it did less: traced always, and either the track or the
        // reason there is none.
        self.status = match watched {
            true => format!("`{name}` traced · {}", self.status),
            false => format!(
                "`{name}` traced — Trace says where it comes from; no dump open, so no track"
            ),
        };
        if watched && !only {
            let under = match path.is_empty() {
                true => open.design.module(module).name.clone(),
                false => path.clone(),
            };
            self.status = format!("{} — under `{under}`, one of several", self.status);
        }

        // And lit in the diagram, when the diagram is showing this module. The
        // click was on a name in a file; the wire it names is the same wire in
        // the picture, and the two views agreeing about what is selected is
        // most of what makes them one tool.
        if let Some(node) = open.flat.nodes.iter().position(|node| node.path == path)
            && let Some(signal) = open.flat.nodes[node].signal(net)
        {
            light_wire(&mut open, signal);
        }
        self.open = Some(open);
    }

    /// Counts the clocks between the two signals the depth pane names.
    ///
    /// Both layers when both are available: the structure always, and the
    /// measurement when a recording is open. The measurement is skipped when
    /// the structure refused — putting a histogram under a refusal invites it
    /// to be read as the answer to the question that was refused.
    fn measure_depth_pane(&mut self) {
        let Some(mut open) = self.open.take() else { return };
        let (from, to) = (open.depth.from.trim().to_string(), open.depth.to.trim().to_string());

        let report = rtlscope_analyse::depth::analyse(&open.design, &from, &to);
        let mut measured = None;
        let mut cross = Vec::new();

        if !report.failed()
            && let Some(wave) = self.wave.as_mut()
            && let Some(clock) = report.clock.clone()
        {
            match wave.measure_latency(&clock, &from, &to) {
                Ok(found) => {
                    cross = rtlscope_wave::cross_check(&report, &found);
                    measured = Some(found);
                }
                // Not a failure of the count — a fact about the recording, and
                // the structural answer above it still stands.
                Err(why) => self.status = format!("measured nothing: {why}"),
            }
        }

        self.status = match (&report.min_stages, &report.max_stages) {
            (Some(min), Some(max)) if min == max => format!("{from} to {to}: {min} clock(s)"),
            (Some(min), Some(max)) => format!("{from} to {to}: {min} to {max} clock(s)"),
            (Some(min), None) => format!("{from} to {to}: at least {min} clock(s)"),
            _ => format!("{from} to {to}: no answer"),
        };
        open.depth.report = Some(report);
        open.depth.measured = measured;
        open.depth.cross = cross;
        self.open = Some(open);
    }

    /// Asks the depth question without a pointer.
    ///
    /// Undocumented, like the other knobs: a view that only exists after two
    /// fields have been typed into is one nobody can check from outside.
    /// `RTLSCOPE_DEPTH="in_data,out_data"`.
    pub fn measure_depth(&mut self, spec: &str) {
        let Some((from, to)) = spec.split_once(',') else {
            self.status = format!("`{spec}` is not `from,to`");
            return;
        };
        if let Some(open) = self.open.as_mut() {
            open.depth.from = from.trim().to_string();
            open.depth.to = to.trim().to_string();
        }
        self.measure_depth_pane();
        self.show(Tab::Pipeline);
    }

    /// Points the source view at a wire, without taking the panel to it.
    ///
    /// The difference from [`RtlScopeApp::show_signal`] is who asked. A `source`
    /// button is a reader saying "take me there", and moving the panel is the
    /// whole of what they wanted. Selecting a track, or clicking a wire in the
    /// diagram, is a reader looking at something — and pulling the panel off
    /// the Lint report they were reading to show them a declaration they did
    /// not ask for would make both gestures worse.
    ///
    /// So this moves the target and nothing else. A reader already on Source
    /// sees it follow; one who opens Source later lands on the right line;
    /// one who never opens it is not interrupted. The status is left alone for
    /// the same reason — a message a second apart from every click is noise.
    fn follow_source(&mut self, signal: SignalId) {
        let Some(mut open) = self.open.take() else { return };
        if let Some((node, net)) = home_of(&open, signal) {
            let module = open.flat.nodes[node].module;
            let span = open.design.module(module).net(net).span;
            open.show_source(span);
        }
        let lit = light_wire(&mut open, signal);
        self.open = Some(open);

        // Said only when it could not be shown. A wire that lit up needs no
        // sentence — the reader is looking at it — and one that did not is a
        // click that appears to have done nothing.
        if let Elsewhere::Inside(where_it_is) = lit {
            self.status = format!("`{where_it_is}` is inside another module; press diagram to go");
        }
    }

    /// Takes a wire the dump recorded back to the design.
    ///
    /// The other direction of [`RtlScopeApp::pick_net`], and the reason a track
    /// remembers its `SignalId` at all: a waveform that cannot say which net it
    /// is leaves the reader to find it by name, which is exactly the work the
    /// matching already did once.
    fn show_signal(&mut self, signal: SignalId, what: Reveal) {
        let Some(mut open) = self.open.take() else { return };
        let Some((node, net)) = home_of(&open, signal) else {
            self.status = "that track is in the dump but not in the design".to_string();
            self.open = Some(open);
            return;
        };
        let (module, path) = (open.flat.nodes[node].module, open.flat.nodes[node].path.clone());
        let name = open.design.module(module).net(net).name.clone();

        match what {
            Reveal::Source => {
                let span = open.design.module(module).net(net).span;
                open.show_source(span);
                self.show(Tab::Source);
                self.status = format!("`{name}` is declared here");
            }
            Reveal::Diagram => {
                // The diagram is the canvas, which is on screen whatever tab is
                // open, so this navigates and lights the wire and leaves the
                // tab where the reader put it.
                open.path = tree::crumbs_for(&open.design, &path);
                open.selected = None;
                open.highlighted.clear();
                open.highlighted.insert(net);
                let module = open.design.module(module).name.clone();
                self.status = format!("`{name}` is lit in {module}");
            }
            Reveal::Trace => {
                let crumbs = tree::crumbs_for(&open.design, &path);
                open.trail = vec![views::Hop { path: crumbs, net }];
                self.show(Tab::Trace);
                self.status = format!("tracing `{name}`");
            }
        }
        self.open = Some(open);
    }

    /// The empty window: a drop target that says what it takes, and whatever
    /// the last read had to say about why it was not a design.
    ///
    /// Drawn in two places — filling the window when there is nothing at all,
    /// and filling the Diagram pane when a recording has been opened without
    /// one — because both are the same question, and answering it twice would
    /// let the two answers drift.
    fn welcome(&mut self, ui: &mut Ui) {
        // Highlighting while files are still in the air is how a drop target
        // says it is one before anything has been let go of.
        let hovering = ui.input(|input| !input.raw.hovered_files.is_empty());
        let failed = self.failed.as_ref().map(|f| (&f.files, &f.diags, f.what.as_str()));
        let tops = self.failed.as_ref().map(|f| f.tops.clone()).unwrap_or_default();
        let asked = views::welcome(ui, hovering, failed, &tops);
        if let Some(what) = asked.browse {
            self.browse(ui.ctx(), what);
        }
        if let Some(span) = asked.open
            && let Some(files) = self.failed.as_ref().map(|f| f.files.clone())
        {
            // Nothing was elaborated, so there is no Source view to send this
            // to: the editor is the only place it can go.
            self.open_in_editor(&files, span);
        }
        if let Some(top) = asked.top {
            self.read_as_top(top);
        }
    }

    /// The block diagram, which used to be the fixed centre of the window.
    ///
    /// It paints its own ground, and the dock is told to leave the margin off
    /// this one pane, so the drawing reaches the seams the way it used to reach
    /// the edges of the panel.
    fn diagram_tab(&mut self, ui: &mut Ui) {
        if self.open.is_none() {
            self.welcome(ui);
            return;
        }
        {
            // Taken out and put back so the drawing code can touch the whole
            // application — the status line, which view is in front — while it
            // holds the design. It is never observed missing: nothing between
            // here and the end of this block reads `self.open`.
            let mut open = self.open.take().expect("just checked");
            let module = open.current();
            // Cloned because painting borrows the app mutably for the status
            // line, and the geometry is only rebuilt when the module changes.
            let geom = open.geom(module).clone();
            let mut scene_rect = *open.scene_rects.entry(module).or_insert(Rect::ZERO);
            let (selected, show_clocks) =
                (open.selected.as_ref().map(|s| s.node), self.show_clocks);

            let action =
                canvas::draw(ui, &geom, &mut scene_rect, show_clocks, selected, &open.highlighted);

            open.scene_rects.insert(module, scene_rect);

            if let Some(node) = action.selected {
                let label = geom
                    .boxes
                    .iter()
                    .find(|item| item.node == node)
                    .map(|item| item.label.clone())
                    .unwrap_or_default();
                let span = rtlscope_graph::block::span_of(&open.design, module, node);
                open.highlighted = geom.nets_of(node).into_iter().collect();
                open.selected = Some(Selected { node, label, span });
            }
            if let Some(node) = action.entered {
                // Stepping inside is only meaningful for an instance that has
                // an inside. Everything else — a process, a port, an IP with no
                // source — answers the same gesture with the RTL it came from,
                // which is the other thing a reader means by "what is this".
                let enterable = matches!(node, BlockNode::Inst(id)
                    if open
                        .design
                        .module(module)
                        .insts
                        .get(id.0 as usize)
                        .is_some_and(|inst| !open.design.module(inst.of).is_blackbox));
                if enterable {
                    self.enter(&mut open, node);
                } else if let Some(span) = action.entered_span {
                    open.show_source(span);
                    self.show(Tab::Source);
                }
            }
            if let Some(net) = action.picked_net {
                // Clicking a wire lights it whether or not a dump is open: the
                // diagram is where a signal is easiest to find.
                open.highlighted.clear();
                open.highlighted.insert(net);
                self.pick_net(&mut open, net);
            }
            self.open = Some(open);
        }
    }

    /// Steps into an instance, if the box clicked is one and its module has an
    /// inside worth showing.
    fn enter(&mut self, open: &mut Open, node: BlockNode) {
        let BlockNode::Inst(id) = node else { return };
        let module = open.design.module(open.current());
        let Some(inst) = module.insts.get(id.0 as usize) else { return };
        let (child, name) = (inst.of, inst.name.clone());

        if open.design.module(child).is_blackbox {
            self.status = format!("`{name}` has no source, so there is nothing inside to show");
            return;
        }
        open.path.push(tree::Crumb { module: child, instance: Some(name) });
        open.selected = None;
        open.highlighted.clear();
    }

    /// A net clicked in the diagram: give it a track, and start a trail at it.
    ///
    /// Both, because a click on a wire is one gesture with two readings —
    /// what does it do, and where does it come from — and having the second
    /// ready costs nothing until the Trace tab is opened.
    fn pick_net(&mut self, open: &mut Open, net: NetId) {
        let crumbs = open.path.clone();
        open.trail = vec![views::Hop { path: crumbs.clone(), net }];
        // A new root is a new picture: the cone is fitted afresh rather than
        // left where the last one was placed.
        open.cone.placement.refit();
        // The third reading of the same click: where in the source is this.
        // Quietly, like the trail — ready when the reader looks, and not in
        // their way until then.
        let stop = self.walk_source(open, net);
        let name = open.design.module(open.current()).net(net).name.clone();

        if self.watch(open, &crumbs, net) {
            self.show(Tab::Wave);
            // Both halves of one click: what the waveform did with it, and
            // where in the source it has put the reader.
            if let Some(stop) = stop {
                self.status = format!("{} · {stop}", self.status);
            }
            return;
        }
        // No dump, so no track — but the click was not wasted.
        // Two facts, and dropping either leaves the click looking like it did
        // less than it did: where the source has gone, and why no track
        // appeared.
        self.status = match stop {
            Some(stop) => format!("{name} — {stop}; no dump open, so no track"),
            None => format!("`{name}` — no dump open; Trace says where it comes from"),
        };
    }

    /// Puts the selected machine's state register on the waveform.
    ///
    /// The diagram already says what the values mean, so this is for the reader
    /// who wants to see them beside everything else that was recorded.
    ///
    /// Through `add_net` with the path [`state_signal_of`] found, rather than
    /// through `watch`, which takes the instance the reader is standing in: a
    /// machine can be shown from outside its own module, and then the crumbs
    /// name the wrong place or no place at all.
    fn watch_state(&mut self) {
        let Some(mut open) = self.open.take() else { return };
        open.ensure_fsms();
        let found = open.state_machines().get(open.fsm.selected).and_then(|fsm| {
            let (_, path) = state_signal_of(&open.flat, fsm, &open.path)?;
            Some((fsm.state, path, fsm.state_name.clone()))
        });
        match found {
            None => self.status = "no state machine is selected".to_string(),
            Some((net, path, name)) => match self.wave.as_mut() {
                Some(wave) => {
                    wave.add_net(&open.design, &open.flat, net, &path);
                    self.status = wave.status.clone();
                    self.show(Tab::Wave);
                }
                None => {
                    self.status =
                        format!("`{name}` — open a dump to see it (drag one in, or --dump)");
                }
            },
        }
        self.open = Some(open);
    }

    /// Moves the cursor to when the selected state is next entered.
    ///
    /// The waveform's `←`/`→` walk a track's own edges; this walks one *value*
    /// of a register that need not be on the panel at all. What a reader in
    /// front of a machine asks is "when is it next in here", and the state they
    /// clicked is which "here" they mean.
    fn seek_next_entry(&mut self) {
        let Some(mut open) = self.open.take() else { return };
        open.ensure_fsms();
        let picked = open.state_machines().get(open.fsm.selected).and_then(|fsm| {
            let state = fsm.states.get(open.fsm.state?)?;
            let (signal, _) = state_signal_of(&open.flat, fsm, &open.path)?;
            Some((signal, state.value, state.name.clone()))
        });
        match picked {
            None => self.status = "click a state first — that is which moment to look for".into(),
            Some((signal, value, name)) => match self.wave.as_mut() {
                None => self.status = format!("`{name}` — no dump open, so there is no when"),
                Some(wave) => {
                    let from = wave.cursor.unwrap_or(0);
                    let found = u64::try_from(value)
                        .ok()
                        .and_then(|value| wave.next_time_holding(signal, from, value));
                    match found {
                        Some(at) => {
                            wave.cursor = Some(at);
                            self.status = format!("{name} at {at}");
                        }
                        None => self.status = format!("{name} is not entered again after here"),
                    }
                }
            },
        }
        self.open = Some(open);
    }

    /// Steps the source view through a net's places, one click at a time.
    ///
    /// A click on a different net starts over at its declaration. A click on
    /// the same one goes to the next place it is driven, and the last wraps
    /// back — so a reader who loses count only has to keep clicking.
    ///
    /// Returns what to say about where they have landed, when there is more
    /// than one place to land.
    fn walk_source(&mut self, open: &mut Open, net: NetId) -> Option<String> {
        let path = tree::instance_path(&open.path);
        let module = open.current();

        let at = match &open.walking {
            // The stops are kept rather than recomputed: tracing a net is work,
            // and a reader clicking four times is asking one question.
            Some(walk) if walk.path == path && walk.net == net && !walk.stops.is_empty() => {
                (walk.at + 1) % walk.stops.len()
            }
            _ => {
                let stops = Walk::of(open, module, net);
                open.walking = Some(Walk { path: path.clone(), net, stops, at: 0 });
                0
            }
        };

        let walk = open.walking.as_mut()?;
        walk.at = at;
        let (span, what) = walk.stops.get(at)?.clone();
        let total = walk.stops.len();
        open.show_source(span);

        // Silent when there is nowhere else to go: a reader who cannot walk
        // does not need to be told which step they are on. Without the net's
        // name, because the caller has somewhere better to put it.
        (total > 1).then(|| format!("{what} ({} of {total}) — click again for the next", at + 1))
    }

    /// Gives a net a waveform track, under the instance path it sits at.
    ///
    /// The path matters as much as the net: a dump knows a signal by where it
    /// is, and one net of a module instantiated twice is two signals.
    fn watch(&mut self, open: &mut Open, crumbs: &[tree::Crumb], net: NetId) -> bool {
        let Some(module) = crumbs.last().map(|crumb| crumb.module) else { return false };
        let name = open.design.module(module).net(net).name.clone();
        let path = tree::instance_path(crumbs);

        let Some(wave) = self.wave.as_mut() else {
            self.status = format!("`{name}` — open a dump to see it (drag one in, or --dump)");
            return false;
        };
        wave.add_net(&open.design, &open.flat, net, &path);
        self.status = wave.status.clone();
        true
    }

    /// A step of a provenance walk.
    ///
    /// Every step that cannot be taken says why rather than doing nothing:
    /// a child with no source, the edge of the design, a port tied to a
    /// constant and a port left unconnected are four different answers, and
    /// the reader is chasing a value precisely because they do not yet know
    /// which one they are looking at.
    fn trace_step(&mut self, step: TraceStep) {
        let Some(mut open) = self.open.take() else { return };
        let Some(here) = open.trail.last().cloned() else {
            self.open = Some(open);
            return;
        };
        match step {
            // At least one hop stays: an empty trail is the tab's empty state,
            // which is not what clicking a crumb asks for.
            TraceStep::Back(keep) => open.trail.truncate(keep.max(1)),
            TraceStep::Watch(net) => {
                let crumbs = here.path.clone();
                self.watch(&mut open, &crumbs, net);
            }
            TraceStep::Follow(net) => self.hop(&mut open, views::Hop { path: here.path, net }),
            TraceStep::Enter { instance, port } => {
                self.trace_into(&mut open, &here, &instance, &port);
            }
            TraceStep::Out { port } => self.trace_out(&mut open, &here, &port),
        }
        self.open = Some(open);
    }

    /// Adds a hop, unless it is the one the trail already ends on.
    fn hop(&mut self, open: &mut Open, hop: views::Hop) {
        let name = open.design.module(hop.module()).net(hop.net).name.clone();
        if open.trail.last() == Some(&hop) {
            self.status = format!("`{name}` is where the trail already is");
            return;
        }
        open.trail.push(hop);
        self.status = format!("tracing `{name}`");
        self.show(Tab::Trace);
    }

    /// One hop inwards: into the child instance whose output this is.
    fn trace_into(&mut self, open: &mut Open, here: &views::Hop, instance: &str, port: &str) {
        let module = open.design.module(here.module());
        let Some(inst) = module.insts.iter().find(|inst| inst.name == instance) else {
            self.status = format!("`{instance}` is not in this module any more");
            return;
        };
        let child = inst.of;
        let inside = open.design.module(child);
        if inside.is_blackbox {
            let of = inside.name.clone();
            self.status = format!("`{of}` has no source, so there is nothing inside to follow");
            return;
        }
        let Some(net) = inside.ports.iter().find(|have| have.name == port).map(|have| have.net)
        else {
            let of = inside.name.clone();
            self.status = format!("`{of}` has no port `{port}` any more");
            return;
        };

        let mut path = here.path.clone();
        path.push(tree::Crumb { module: child, instance: Some(instance.to_string()) });
        self.hop(open, views::Hop { path, net });
    }

    /// One hop outwards: through this module's input, to what the parent puts
    /// on it.
    fn trace_out(&mut self, open: &mut Open, here: &views::Hop, port: &str) {
        let mut path = here.path.clone();
        let leaving = match path.len() > 1 {
            true => path.pop().expect("more than one crumb"),
            false => {
                self.status =
                    format!("`{port}` is a port of the top: what drives it is outside the design");
                return;
            }
        };
        let instance = leaving.instance.clone().unwrap_or_default();
        let parent = path.last().expect("more than one crumb was popped from").module;

        let child = open.design.module(leaving.module);
        let Some(port_id) = child.ports.iter().position(|have| have.name == port) else {
            let of = child.name.clone();
            self.status = format!("`{of}` has no port `{port}` any more");
            return;
        };
        let up = open.design.module(parent);
        let Some(inst) = up.insts.iter().find(|inst| inst.name == instance) else {
            let of = up.name.clone();
            self.status = format!("`{instance}` is not in `{of}` any more");
            return;
        };
        // Taken out as values, so nothing of the design is still borrowed when
        // the trail is written to.
        let found = inst.conns.iter().find(|conn| conn.port.0 as usize == port_id);
        let connected = found.is_some();
        let target = found.and_then(|conn| conn.net.net_id());

        match target {
            Some(net) => self.hop(open, views::Hop { path, net }),
            None if connected => {
                self.status = format!("`{instance}.{port}` is tied to a constant here");
            }
            None => self.status = format!("`{instance}.{port}` is left unconnected"),
        }
    }
}

/// A net's places in the source, and which one is on screen.
///
/// A wire is declared once and driven from wherever it is driven, and a reader
/// asking "where is this set" wants all of them. One click cannot show several
/// lines, so clicking again goes to the next and the last wraps back to the
/// declaration — which also means the walk cannot get lost: another click
/// always comes back round.
struct Walk {
    /// The instance path and net this belongs to. A click on anything else
    /// starts a new walk rather than carrying this one's position over.
    path: String,
    net: NetId,
    /// Every place, and what it is, in the order they are visited.
    stops: Vec<(Span, String)>,
    /// Which of them is on screen.
    at: usize,
}

impl Walk {
    /// Where a net is declared, and everywhere it is driven.
    ///
    /// The declaration first, because that is what a reader means by "where is
    /// this" before they mean anything else. Drivers in the order the analysis
    /// found them, which is source order.
    fn of(open: &Open, module: ModuleId, net: NetId) -> Vec<(Span, String)> {
        use rtlscope_analyse::drive::{self, Kind};

        let design_module = open.design.module(module);
        let declared = design_module.net(net).span;

        // Every statement that writes it, at statement grain rather than
        // process grain. A register assigned in a reset branch and again in a
        // data branch is written in two places, and stopping at the `always_ff`
        // header once would leave the reader to find them by eye — which is the
        // work they clicked to avoid.
        let mut rest: Vec<(Span, String)> = drive::assignments(design_module, net)
            .into_iter()
            .map(|span| (span, "assigned".to_string()))
            .collect();

        // And the drivers that are not statements at all. A port and an
        // instance output are places a value comes from with nothing in this
        // module assigning them, so nothing above found them.
        let trace = drive::trace(&open.design, &open.flat, module, net);
        for driver in trace.drivers {
            let what = match &driver.kind {
                Kind::FromOutside { port } => format!("comes in through {port}"),
                Kind::FromInstance { instance, port, .. } => {
                    format!("driven by {instance}.{port}")
                }
                // A process, which the statements above already cover, at a
                // finer grain than the process's own span.
                _ => continue,
            };
            rest.push((driver.span, what));
        }

        // Source order, so clicking through reads down the file the way the
        // file does.
        rest.sort_by_key(|(span, _)| (span.file, span.line, span.col));

        let mut stops = vec![(declared, "declared".to_string())];
        for stop in rest {
            // A stop on the line the reader is already on is a click that does
            // nothing, which reads as the walk being broken.
            if stops.iter().all(|(at, _)| *at != stop.0) {
                stops.push(stop);
            }
        }
        stops
    }
}

/// What a track's wire is being asked to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reveal {
    Diagram,
    Source,
    Trace,
}

/// Where a wire turned out to be, when something asked for it to be lit.
enum Elsewhere {
    /// It is in the module being drawn, and is now lit.
    Lit,
    /// It exists, but in a module the diagram is not showing.
    Inside(String),
    /// The design does not have it under any name.
    Nowhere,
}

/// Lights a wire in the diagram, if the diagram is showing the module it is in.
///
/// Deliberately without navigating. Selecting a track or a name is a reader
/// looking at something, not asking to be taken anywhere — and a selection that
/// silently walked the breadcrumb into a child module would make the diagram
/// jump under the hand of somebody who was reading it. The `diagram` button is
/// the gesture that means "take me there", and it still does.
///
/// So this lights what is on screen and says where the rest is.
fn light_wire(open: &mut Open, signal: SignalId) -> Elsewhere {
    let here = tree::instance_path(&open.path);
    let homes = open.flat.homes(signal);

    if let Some((_, net)) =
        homes.iter().find(|(node, _)| open.flat.nodes[*node].path == here).copied()
    {
        open.highlighted.clear();
        open.highlighted.insert(net);
        return Elsewhere::Lit;
    }

    // Not in this module. The shallowest name is the one a reader is most
    // likely to recognise, which is the same choice `home_of` makes.
    match homes.iter().min_by_key(|(node, _)| open.flat.nodes[*node].path.len()) {
        Some(_) => Elsewhere::Inside(open.flat.name_of(signal)),
        None => Elsewhere::Nowhere,
    }
}

/// How big the main window was left, if that was ever written down.
///
/// Free rather than a method, because the size is wanted before there is an
/// application to ask: a window is built from `NativeOptions` and the first
/// frame happens inside it.
pub fn saved_main_size() -> Option<[f32; 2]> {
    crate::layout::Layout::read().main_size
}

/// What the dock draws through.
///
/// It holds the whole application, because a view is not a self-contained
/// widget here: drawing the FSM can move the source, and drawing the waveform
/// can start a simulation. The dock itself is out of the application while this
/// exists — see `RtlScopeApp::desk` — which is what makes the two borrows
/// disjoint, and why `show` writes its move down rather than making it.
struct Viewer<'a> {
    app: &'a mut RtlScopeApp,
    theme: &'static Theme,
}

impl TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        self.app.title_of(*tab).into()
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Tab) {
        self.app.view(ui, *tab);
    }

    /// The name the tab's contents are remembered by, between frames and
    /// across moves.
    ///
    /// Overridden because the default is the title, and two of those change as
    /// the window is used — the waveform names its recording, the diagnostics
    /// mark themselves when the sources move on. A pane whose identity changed
    /// with its label would lose its scroll position the moment a dump was
    /// opened.
    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(("rtlscope-tab", tab.key()))
    }

    /// No scroll bars from the dock.
    ///
    /// Every view that scrolls brings its own scroll area, sized to the space
    /// it was given. A scrolling container around them would offer the space
    /// below as infinite, and the three views that measure what they were given
    /// before drawing on it — the diagram, the cone, the state machine — would
    /// have nothing to fit into.
    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }

    /// The diagram gets the whole pane and its own ground.
    ///
    /// It paints a sheet, and a sheet inset by four pixels of a different
    /// colour reads as a mistake rather than as a margin.
    fn tab_style_override(
        &self,
        tab: &Tab,
        global: &egui_dock::TabStyle,
    ) -> Option<egui_dock::TabStyle> {
        (*tab == Tab::Diagram).then(|| {
            let mut style = global.clone();
            style.tab_body.inner_margin = egui::Margin::ZERO;
            style.tab_body.bg_fill = self.theme.canvas_bg;
            style
        })
    }
}

/// The signal a trail hop is standing on.
///
/// A hop names a net in a module; the cone speaks in signals, which are what a
/// net becomes once the hierarchy has been flattened and the same wire on both
/// sides of a boundary has become one thing.
fn signal_of_hop(flat: &rtlscope_analyse::flat::Flattened, hop: &views::Hop) -> Option<SignalId> {
    let here = tree::instance_path(&hop.path);
    flat.nodes.iter().find(|node| node.path == here).and_then(|node| node.signal(hop.net))
}

/// A net for a signal, in the module being looked at if it has one there.
///
/// The mirror of [`home_of`], which asks the same question of the diagram. A
/// cone reaches across boundaries, so the signal it hands back may not have a
/// net in the module on screen; the shallowest name is then the one a reader is
/// most likely to recognise, which is the choice made everywhere else.
fn home_of_signal(open: &Open, signal: SignalId) -> Option<(usize, NetId)> {
    let here = tree::instance_path(&open.path);
    let homes = open.flat.homes(signal);
    homes
        .iter()
        .find(|(node, _)| open.flat.nodes[*node].path == here)
        .or_else(|| homes.iter().min_by_key(|(node, _)| open.flat.nodes[*node].path.len()))
        .copied()
}

/// Which of a wire's names to show it under.
///
/// A signal wears one on each side of every boundary it crosses. The one in the
/// module being drawn is the answer when there is one, since that is what the
/// reader is already looking at; otherwise the outermost, which is the name
/// they are most likely to recognise. Anything else drops them into a module
/// they were not in to show them a net they did not name.
fn home_of(open: &Open, signal: SignalId) -> Option<(usize, NetId)> {
    let here = tree::instance_path(&open.path);
    let homes = open.flat.homes(signal);
    homes
        .iter()
        .find(|(node, _)| open.flat.nodes[*node].path == here)
        .or_else(|| homes.iter().min_by_key(|(node, _)| open.flat.nodes[*node].path.len()))
        .copied()
}

/// The deepest clock domain of a design that the dump actually recorded.
/// What a toolbar button says when it needs the design and there is none.
///
/// Not silence, and not a hidden button. A recording opened on its own draws
/// the same toolbar as one opened beside sources — half a toolbar would leave
/// the reader guessing what this viewer is missing — so a press has to answer,
/// and the answer names the thing that would make it work.
fn needs_a_design(what: &str) -> String {
    format!(
        "{what} needs the design — drop the sources in and they will be read against this          recording"
    )
}

fn lay_out_stages(design: &Design, state: &mut crate::wave::WaveState) {
    // The domains come deepest first, so the one laid out is the deepest whose
    // clock the dump has.
    let report = rtlscope_analyse::pipeline::analyse(design);
    let found = report.domains.into_iter().find(|d| state.matches.by_ir_name(&d.clock).is_some());
    match found {
        Some(domain) => state.open_stages(domain),
        None => {
            state.status = "no clock domain of this design is in the dump, so there are no                             cycles to lay the stages against"
                .to_string();
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// What a folder was read as, in one sentence.
///
/// Said out loud because a folder drop is the one input where the reader
/// cannot see what was taken: forty files went in and one design came out, and
/// if the wrong subtree was picked the diagnostics are about files they never
/// meant to open.
pub(crate) fn describe(root: &Path, found: &Gathered) -> String {
    if found.sources.is_empty() {
        return format!("no .sv or .v under {}", file_name(root));
    }
    let mut said = format!("{} source(s) from {}", found.sources.len(), file_name(root));
    if !found.includes.is_empty() {
        let _ = write!(said, ", {} include dir(s)", found.includes.len());
    }
    if !found.skipped.is_empty() {
        let _ = write!(said, ", skipped {}", found.skipped.join(" "));
    }
    said
}

/// What a folder turned out to hold.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Gathered {
    /// Every `.sv` and `.v` under it, in a fixed order.
    pub sources: Vec<PathBuf>,
    /// Directories holding headers, for the include path.
    pub includes: Vec<PathBuf>,
    /// Directories deliberately not walked, named so the reader can see they
    /// were left out rather than missed.
    pub skipped: Vec<String>,
}

/// Directories not walked, and why.
///
/// `rtlscope-sim` is where this program writes its own generated harnesses. A
/// folder drop that swept those back in would hand the elaborator a second
/// definition of every module under a `tb_` wrapper, and the diagnostics that
/// came back would be about RTLScope rather than about the design.
fn skip_this(name: &str) -> bool {
    name.starts_with('.') || name == "rtlscope-sim" || name == "target" || name == "node_modules"
}

/// Everything under a folder that a design could be read from.
///
/// Headers are not sources. A `.svh` is meant to be pulled in by an `include`
/// from somewhere else, and compiling one on its own produces errors about a
/// fragment nobody asked to compile; so the header itself is left out and the
/// directory holding it is offered as an include path instead, which is what
/// the file was written expecting.
///
/// The order is sorted rather than whatever the filesystem hands back, because
/// a design read twice has to come out the same both times — the module the
/// elaborator picks as the top depends on what it saw.
pub(crate) fn gather_sources(root: &Path) -> Gathered {
    let mut found = Gathered::default();
    let mut includes = std::collections::BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        let mut here: Vec<PathBuf> = entries.filter_map(|entry| Some(entry.ok()?.path())).collect();
        here.sort();
        for path in here {
            if path.is_dir() {
                let name = file_name(&path);
                match skip_this(&name) {
                    true => found.skipped.push(name),
                    false => stack.push(path),
                }
                continue;
            }
            let extension = path.extension().map(|it| it.to_string_lossy().to_ascii_lowercase());
            match extension.as_deref() {
                Some("sv" | "v") => found.sources.push(path),
                Some("svh" | "vh") => {
                    if let Some(parent) = path.parent() {
                        includes.insert(parent.to_path_buf());
                    }
                }
                _ => {}
            }
        }
    }

    found.sources.sort();
    found.skipped.sort();
    found.includes = includes.into_iter().collect();
    found
}

/// What a dropped file is taken to be.
///
/// By extension, which is the only thing available before reading it, and
/// case-insensitively because Windows hands over whatever the user typed when
/// they saved it. Anything unrecognised is [`Dropped::Unknown`] rather than
/// guessed at: reading a `.txt` as SystemVerilog would produce a page of
/// syntax errors instead of the one sentence that is actually true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dropped {
    Source,
    FileList,
    Dump,
    Results,
    Folder,
    Unknown,
}

impl Dropped {
    pub(crate) fn of(path: &Path) -> Self {
        // Before the extension, because a directory may have a dot in its name
        // and `verify.v/` is a folder, not a Verilog file.
        if path.is_dir() {
            return Dropped::Folder;
        }
        // A `Veryl.toml` is a source in the sense that matters: it names a
        // design to read, and reading it is what `load` does with it.
        if rtlscope_veryl::is_veryl(path) {
            return Dropped::Source;
        }
        let extension = path.extension().map(|ext| ext.to_string_lossy().to_ascii_lowercase());
        match extension.as_deref() {
            Some("sv" | "v" | "svh" | "vh") => Dropped::Source,
            Some("f") => Dropped::FileList,
            Some("vcd" | "fst") => Dropped::Dump,
            Some("xml") => Dropped::Results,
            _ => Dropped::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(name: &str) -> Dropped {
        Dropped::of(Path::new(name))
    }

    /// A project folder holds several designs, so the elaborator cannot guess
    /// which is meant. On the command line the answer is `--top`; a window that
    /// only repeated that would be telling somebody to close it and start
    /// again, so the failure has to carry the names it could offer.
    #[test]
    fn a_folder_with_several_designs_offers_the_tops_it_found() {
        let dir = std::env::temp_dir().join("rtlscope-two-tops");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("makes the tree");
        let path = dir.join("both.sv");
        std::fs::write(
            &path,
            "module leaf (input logic clk, output logic y);
             always_ff @(posedge clk) y <= ~y;
             endmodule
             module one (input logic clk, output logic y);
             leaf u (.clk (clk), .y (y));
             endmodule
             module two (input logic clk, output logic y);
             leaf u (.clk (clk), .y (y));
             endmodule
",
        )
        .expect("writes");

        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.load(std::slice::from_ref(&path), false);

        let failed = app.failed.as_ref().expect("two tops is not a design");
        assert_eq!(failed.tops, ["one", "two"], "in declaration order: {:?}", failed.tops);
        assert!(!failed.tops.iter().any(|name| name == "leaf"), "something instantiates leaf");
        assert_eq!(failed.paths, [path], "and it kept what to read again");

        // Choosing one reads the same files again and opens that design.
        app.read_as_top("two".to_string());
        let open = app.open.as_ref().expect("choosing a top opens it");
        assert_eq!(open.design.module(open.design.top).name, "two");
        assert!(app.failed.is_none(), "and the failure is gone");
    }

    /// A board-level top is made of vendor primitives, so this is the common
    /// case and not an edge one. Saying so takes a second; letting a simulator
    /// discover it takes a build and answers with a compiler error naming a
    /// file the reader did not write.
    #[test]
    fn a_design_with_no_source_for_a_child_is_refused_by_name() {
        let dir = std::env::temp_dir().join("rtlscope-blackbox");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("makes the tree");
        let path = dir.join("board.sv");
        std::fs::write(
            &path,
            "module clean (input logic clk, output logic y);
             always_ff @(posedge clk) y <= ~y;
             endmodule
             module board (input logic clk, output logic y);
             MMCME2_BASE u_mmcm (.CLKIN1 (clk));
             clean u_clean (.clk (clk), .y (y));
             endmodule
",
        )
        .expect("writes");

        let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
        let design = rtlscope_elab::elaborate(&uir, Some("board")).0.expect("elaborates");

        let (board, _) = design.module_by_name("board").expect("the top is there");
        let said = unbuildable(&design, board, "board").expect("it cannot be built");
        assert!(said.contains("MMCME2_BASE"), "named, not counted: {said}");

        // And the advice it gives has to be true: `clean` reaches no black box,
        // so simulating it must not be refused.
        let (clean, _) = design.module_by_name("clean").expect("the child is there");
        assert_eq!(unbuildable(&design, clean, "clean"), None);
    }

    /// A simulation compiles what the design needs, not what was opened.
    ///
    /// A dropped folder brings in everything under it, and the modules the top
    /// never instantiates are exactly where the syntax a simulator chokes on
    /// tends to live: measured on a real project, Icarus refused forty-two
    /// files with sixty complaints, none of them about a module in the design.
    #[test]
    fn a_simulation_is_given_only_the_files_the_design_reaches() {
        let dir = std::env::temp_dir().join("rtlscope-needed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("makes the tree");

        let write = |name: &str, text: &str| {
            let at = dir.join(name);
            std::fs::write(&at, text).expect("writes");
            at
        };
        let top = write(
            "top.sv",
            "module top (input logic clk, output logic y);\n\
             leaf u_leaf (.clk (clk), .y (y));\n\
             endmodule\n",
        );
        let leaf = write(
            "leaf.sv",
            "module leaf (input logic clk, output logic y);\n\
             always_ff @(posedge clk) y <= ~y;\n\
             endmodule\n",
        );
        // Nobody instantiates this, and it is the one a compiler would trip on.
        let stranger = write(
            "stranger.sv",
            "module stranger (input logic clk);\n\
             endmodule\n",
        );
        // No module at all: this is where a compilation-unit typedef lives, so
        // it has to survive even though nothing points at it.
        let shared = write("shared.sv", "typedef logic [7:0] byte_t;\n");

        // `leaf` is spelled the long way round on purpose: the frontend records
        // the path it opened rather than the one it was handed, and a
        // comparison that took the spelling at face value would match nothing
        // and keep everything.
        let leaf_said = dir.join(".").join("leaf.sv");
        let given = vec![top.clone(), leaf_said.clone(), stranger.clone(), shared.clone()];
        let (uir, _) = rtlscope_sv::lower_files(&given, &ParseOptions::default());
        let design = rtlscope_elab::elaborate(&uir, Some("top")).0.expect("elaborates");

        let kept = needed(&uir, &design, &given);
        assert!(kept.contains(&top), "the top: {kept:?}");
        assert!(kept.contains(&leaf_said), "and what it instantiates: {kept:?}");
        let _ = &leaf;
        assert!(kept.contains(&shared), "and anything with no module in it: {kept:?}");
        assert!(!kept.contains(&stranger), "but not a module nothing reaches: {kept:?}");
        // The order a compiler reads them in is the order it was given.
        assert_eq!(kept, vec![top, leaf_said, shared]);
    }

    /// A little project on disk: nested sources, a header, and two directories
    /// that must not be walked.
    fn a_project(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&root);
        for directory in ["rtl/core", "rtl/include", ".git", "rtlscope-sim"] {
            std::fs::create_dir_all(root.join(directory)).expect("makes the tree");
        }
        for file in [
            "top.sv",
            "rtl/alu.v",
            "rtl/core/regfile.sv",
            "rtl/include/defs.svh",
            "rtl/include/legacy.vh",
            ".git/hook.sv",
            "rtlscope-sim/tb_top.sv",
            "notes.txt",
        ] {
            std::fs::write(root.join(file), "// nothing in particular\n").expect("writes");
        }
        root
    }

    /// The point of the whole thing: a real project is a tree, and a debugger
    /// that made somebody list it file by file would not get used on one.
    #[test]
    fn a_folder_gives_up_every_source_under_it() {
        let root = a_project("rtlscope-gather-all");
        let found = gather_sources(&root);

        let names: Vec<String> = found.sources.iter().map(|path| file_name(path)).collect();
        assert_eq!(names, ["alu.v", "regfile.sv", "top.sv"], "nested and sorted: {names:?}");
    }

    /// A header is not a source. Compiling one on its own produces errors about
    /// a fragment nobody asked to compile; the directory holding it is what the
    /// file that includes it needs.
    #[test]
    fn a_header_is_not_a_source_but_its_directory_is_an_include() {
        let root = a_project("rtlscope-gather-headers");
        let found = gather_sources(&root);

        assert!(
            !found.sources.iter().any(|path| file_name(path).ends_with('h')),
            "no headers among the sources: {:?}",
            found.sources
        );
        assert_eq!(found.includes, vec![root.join("rtl").join("include")]);
    }

    /// `rtlscope-sim` holds this program's own generated harnesses. Sweeping them
    /// back in would hand the elaborator a second definition of every module,
    /// and the diagnostics would be about RTLScope rather than the design.
    #[test]
    fn the_generated_and_the_hidden_are_left_out_and_said_so() {
        let root = a_project("rtlscope-gather-skips");
        let found = gather_sources(&root);

        let names: Vec<String> = found.sources.iter().map(|path| file_name(path)).collect();
        assert!(!names.contains(&"tb_top.sv".to_string()), "not the harnesses: {names:?}");
        assert!(!names.contains(&"hook.sv".to_string()), "not the dot directories: {names:?}");
        assert_eq!(found.skipped, [".git", "rtlscope-sim"], "and it says which: {found:?}");
        assert!(describe(&root, &found).contains("skipped"), "out loud, too");
    }

    /// Read twice, the same both times. Which module the elaborator takes as the
    /// top depends on what it saw, so an order that came from the filesystem
    /// would make a design open differently on two machines.
    #[test]
    fn the_same_folder_reads_the_same_way_twice() {
        let root = a_project("rtlscope-gather-order");
        assert_eq!(gather_sources(&root), gather_sources(&root));
    }

    /// An empty folder is a mistake worth naming. Silence looks like the drop
    /// was not noticed.
    #[test]
    fn a_folder_with_nothing_in_it_says_so() {
        let root = std::env::temp_dir().join("rtlscope-gather-empty");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs")).expect("makes the tree");
        std::fs::write(root.join("README.md"), "nothing here\n").expect("writes");

        let found = gather_sources(&root);
        assert!(found.sources.is_empty());
        assert!(
            describe(&root, &found).starts_with("no .sv or .v under"),
            "{}",
            describe(&root, &found)
        );
    }

    /// Dropped before the extension is looked at, because a directory may have
    /// a dot in its name.
    #[test]
    fn a_directory_is_a_folder_whatever_it_is_called() {
        let root = std::env::temp_dir().join("rtlscope-gather-dotted.v");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("makes it");
        assert_eq!(Dropped::of(&root), Dropped::Folder);
    }

    /// A design read from a fixture, in a window nobody is looking at.
    fn opened(fixture: &str) -> RtlScopeApp {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.load(&[rtlscope_fixtures::path(fixture)], false);
        assert!(app.open.is_some(), "{fixture} reads as a design");
        app
    }

    fn net_of(design: &Design, module: ModuleId, name: &str) -> NetId {
        design
            .module(module)
            .nets
            .iter_enumerated()
            .find(|(_, net)| net.name == name)
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("`{name}` is a net of this module"))
    }

    /// The trail, as a reader would read it off the breadcrumb.
    fn trail_of(app: &RtlScopeApp) -> Vec<String> {
        let open = app.open.as_ref().expect("open");
        open.trail
            .iter()
            .map(|hop| {
                let path = crate::tree::instance_path(&hop.path);
                let name = &open.design.module(hop.module()).net(hop.net).name;
                match path.is_empty() {
                    true => name.clone(),
                    false => format!("{path}.{name}"),
                }
            })
            .collect()
    }

    /// A drawing outlives the window that made it.
    ///
    /// This is the one thing in here somebody authored rather than derived, and
    /// until it could be written down it existed only in memory — a testcase
    /// that took real work to draw, gone when the window closed. The file it
    /// writes is the same one `rtlscope sim --pattern` reads, so saving it is
    /// also what makes it runnable from the terminal.
    #[test]
    fn a_drawing_can_be_written_down_and_read_back() {
        let path = scratch_of("pipeline3.sv", "stim");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.load(std::slice::from_ref(&path), false);
        let module = app.drawing_for().expect("a module to draw against");

        app.draw_demo();
        let drawn = app.patterns.get(&module).cloned().expect("something was drawn");
        assert!(!drawn.drive.is_empty(), "there are lanes to lose");

        app.save_pattern(&module);
        let written = app.pattern_path(&module).expect("a place for it");
        assert!(written.exists(), "{}", app.status);

        // A fresh window, the way it would be after closing this one.
        let mut again = RtlScopeApp::new(ParseOptions::default(), None, None);
        again.load(std::slice::from_ref(&path), false);
        assert!(again.patterns.is_empty(), "nothing is remembered by itself");

        again.load_pattern(&module);
        let back = again.patterns.get(&module).expect("read back");
        assert_eq!(back.drive, drawn.drive, "the same drawing came back");
        assert_eq!(back.expect, drawn.expect);
        assert_eq!(back.cycles, drawn.cycles);

        // And it is the file the terminal reads, not a private format.
        let text = std::fs::read_to_string(&written).expect("readable");
        let parsed = rtlscope_tb::Pattern::from_json(&text).expect("`sim --pattern` reads this");
        assert_eq!(parsed.drive, drawn.drive);

        let _ = std::fs::remove_file(&written);
        let _ = std::fs::remove_file(&path);
    }

    /// Saving somewhere unwritable says where it tried.
    #[test]
    fn a_drawing_that_cannot_be_written_says_where_it_tried() {
        let path = scratch_of("pipeline3.sv", "stim-nowhere");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.load(std::slice::from_ref(&path), false);
        let module = app.drawing_for().expect("a module");
        app.draw_demo();

        // A directory where the file should be: writing it cannot succeed.
        let blocked = app.pattern_path(&module).expect("a place for it");
        let _ = std::fs::remove_file(&blocked);
        std::fs::create_dir_all(&blocked).expect("a directory in the way");

        app.save_pattern(&module);

        assert!(app.status.contains("could not write"), "{}", app.status);
        assert!(
            app.status.contains(&blocked.file_name().unwrap().to_string_lossy().to_string()),
            "and it says which file: {}",
            app.status
        );

        let _ = std::fs::remove_dir(&blocked);
        let _ = std::fs::remove_file(&path);
    }

    /// A design with the recording of it already open.
    fn watched(fixture: &str, dump: &str) -> RtlScopeApp {
        let mut app = opened(fixture);
        app.open_dump(rtlscope_fixtures::dir().join(dump));
        assert!(app.wave.is_some(), "{dump} opened: {}", app.status);
        app
    }

    /// A design with its machines found, its recording open, and the cursor
    /// wherever the caller wants it.
    fn machine_at(at: u64) -> RtlScopeApp {
        let mut app = watched("fsm.sv", "waves/fsm.vcd");
        app.open.as_mut().expect("open").ensure_fsms();
        app.wave.as_mut().expect("a dump").cursor = Some(at);
        app
    }

    /// The waveform's answer to the machine's question.
    ///
    /// The whole of the projection: the reader moves the cursor and the diagram
    /// says which state that is, instead of leaving them to read `b01` off a
    /// row and translate it in their head. The times come from `fsm.vcd`, whose
    /// posedges are at 10, 30, 50, 70, 90.
    #[test]
    fn the_cursor_says_which_state_the_machine_is_in() {
        let mut app = machine_at(0);
        let (Some(open), Some(wave)) = (app.open.as_ref(), app.wave.as_mut()) else {
            panic!("both are open");
        };
        let fsm = &open.state_machines()[0];

        for (at, want) in [(0, "S_IDLE"), (50, "S_RUN"), (70, "S_WAIT"), (90, "S_IDLE")] {
            wave.cursor = Some(at);
            let now =
                now_for(fsm, &open.flat, &open.path, Some(&mut *wave)).expect("a reading at {at}");
            let name = now.state.map(|index| fsm.states[index].name.as_str());
            assert_eq!(name, Some(want), "at {at}");
            assert_eq!(now.at, at, "and it says which moment it read");
        }
    }

    /// No cursor is no reading. The diagram is drawn plain rather than showing
    /// the machine parked in whatever state the dump happens to open on.
    #[test]
    fn without_a_cursor_the_machine_is_drawn_plain() {
        let mut app = machine_at(0);
        let (Some(open), Some(wave)) = (app.open.as_ref(), app.wave.as_mut()) else {
            panic!("both are open");
        };
        wave.cursor = None;
        let fsm = &open.state_machines()[0];
        assert!(now_for(fsm, &open.flat, &open.path, Some(wave)).is_none());
    }

    /// `next ▸` is the gesture the waveform's arrow keys cannot do: they walk
    /// one track's edges, and this walks one *value* of a register that need
    /// not be on the panel at all. In `fsm.vcd`, `S_WAIT` is entered at 70.
    #[test]
    fn next_moves_the_cursor_to_when_that_state_is_entered() {
        let mut app = machine_at(0);
        let open = app.open.as_mut().expect("open");
        let wait = open.state_machines()[0]
            .states
            .iter()
            .position(|state| state.name == "S_WAIT")
            .expect("the machine has S_WAIT");
        open.fsm.state = Some(wait);

        app.seek_next_entry();
        assert_eq!(app.wave.as_ref().and_then(|w| w.cursor), Some(70), "{}", app.status);

        // And from past the only entry there is nowhere to go, which is said
        // rather than done silently.
        app.seek_next_entry();
        assert_eq!(app.wave.as_ref().and_then(|w| w.cursor), Some(70), "{}", app.status);
        assert!(app.status.contains("not entered again"), "{}", app.status);
    }

    /// Watching puts the register on the panel under the name the dump knows
    /// it by, so the row and the diagram are two readings of one signal.
    #[test]
    fn watching_a_machine_puts_its_register_on_the_panel() {
        let mut app = machine_at(0);
        app.watch_state();

        let wave = app.wave.as_ref().expect("a dump");
        let names: Vec<&str> = wave
            .tracks
            .iter()
            .filter_map(|track| match track {
                crate::wave::Track::Signal { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            names.iter().any(|name| name.ends_with(".state")),
            "the state register is there: {names:?} — {}",
            app.status
        );
    }

    /// A recording on its own is a thing to look at.
    ///
    /// The commonest way anybody meets a VCD is somebody sending them one, and
    /// for a long time dropping it here did nothing visible: the file was held
    /// against the arrival of sources that, in that situation, do not exist.
    #[test]
    fn a_recording_dropped_on_its_own_opens_the_viewer() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        assert!(app.open.is_none(), "no design, which is the point");

        app.open_dump(rtlscope_fixtures::wave("trace_demo.vcd"));

        let wave = app.wave.as_ref().expect("the dump opened");
        assert!(!wave.tracks.is_empty(), "and it opened showing something: {}", app.status);
        assert!(app.is_visible(Tab::Wave), "and it is on the desk where the reader can see it");
    }

    /// And the sources can arrive second.
    ///
    /// Which is the order that actually happens: the recording is what was
    /// sent, the RTL is what gets found afterwards. Every row already on the
    /// panel has to pick up the net it turns out to be, or the second half of
    /// the viewer — the way back to the diagram and the source — stays shut
    /// for everything the reader was already looking at.
    #[test]
    fn sources_arriving_after_the_recording_name_its_rows() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_dump(rtlscope_fixtures::wave("trace_demo.vcd"));
        let before = app.wave.as_ref().expect("the dump opened");
        assert!(before.matches.matched.is_empty(), "nothing to match against yet");
        let rows = before.tracks.len();

        app.load(&[rtlscope_fixtures::path("trace_demo.sv")], false);

        let wave = app.wave.as_ref().expect("still open");
        assert_eq!(wave.tracks.len(), rows, "the same rows, not a fresh panel");
        assert!(!wave.matches.matched.is_empty(), "matched now: {}", app.status);
        assert!(
            wave.tracks
                .iter()
                .any(|track| matches!(track, crate::wave::Track::Signal { signal: Some(_), .. })),
            "and a row knows which net it is: {}",
            app.status
        );
    }

    /// A name clicked in the source becomes a track.
    ///
    /// The source view has a file and some lines and no idea which net any of
    /// it is, so everything about this happens on the application's side: which
    /// module the lines belong to, which net wears the name, and which instance
    /// of that module the reader is looking at.
    #[test]
    fn a_name_in_the_source_can_be_put_on_the_waveform() {
        let mut app = watched("trace_demo.sv", "waves/trace_demo.vcd");

        let before = app.wave.as_ref().map(|wave| wave.tracks.len()).expect("a dump is open");

        app.pick_named("staged");

        let after = app.wave.as_ref().map(|wave| wave.tracks.len()).expect("still open");
        assert_eq!(after, before + 1, "a track was added: {}", app.status);
    }

    /// Asking for a track that is already there is not adding one, and saying
    /// otherwise leaves a reader looking for a row that did not appear. It
    /// selects the row instead, so the answer to "where is it" is on screen.
    #[test]
    fn asking_twice_says_it_is_already_there_and_shows_where() {
        let mut app = watched("trace_demo.sv", "waves/trace_demo.vcd");
        app.pick_named("staged");
        let after_first = app.wave.as_ref().map(|wave| wave.tracks.len()).expect("a dump is open");

        app.pick_named("staged");

        let wave = app.wave.as_ref().expect("still open");
        assert_eq!(wave.tracks.len(), after_first, "no second copy");
        assert!(app.status.contains("already"), "and it said so: {}", app.status);
        assert!(wave.selected().is_some(), "the row it means is selected");
    }

    /// A wire clicked in the diagram points the source view at its
    /// declaration. Quietly: the panel stays on whatever was open, because the
    /// click was about the diagram.
    #[test]
    fn a_wire_clicked_in_the_diagram_points_the_source_at_it() {
        let mut app = opened("trace_demo.sv");
        app.show(Tab::Lint);

        let (net, line) = {
            let open = app.open.as_ref().expect("a design");
            let module = open.design.module(open.current());
            let net = module
                .nets
                .indices()
                .find(|id| module.net(*id).name == "staged")
                .expect("`staged` is a net of the top");
            (net, module.net(net).span.line)
        };

        let mut open = app.open.take().expect("a design");
        app.pick_net(&mut open, net);
        app.open = Some(open);

        let open = app.open.as_ref().expect("still open");
        assert_eq!(open.source_span().line, line, "the source view moved to `staged`");
        assert!(app.is_visible(Tab::Lint), "and the report is still in view");
    }

    /// Clicking the same wire again walks its places in the source: declared,
    /// then everywhere it is driven, then round to the declaration. `staged` is
    /// declared once and assigned in one clocked process, so the walk is two
    /// stops long and the third click is back where it started.
    #[test]
    fn clicking_a_wire_again_walks_to_where_it_is_driven() {
        let mut app = opened("trace_demo.sv");

        let (net, declared) = {
            let open = app.open.as_ref().expect("a design");
            let module = open.design.module(open.current());
            let net = module
                .nets
                .indices()
                .find(|id| module.net(*id).name == "staged")
                .expect("`staged` is a net of the top");
            (net, module.net(net).span.line)
        };

        let mut open = app.open.take().expect("a design");
        app.pick_net(&mut open, net);
        assert_eq!(open.source_span().line, declared, "the first click declares it");

        // `staged` is written twice in one process — once in the reset branch
        // and once in the data branch — so the walk has to be three stops, not
        // two. Stopping once at the `always_ff` header is the thing this test
        // exists to prevent.
        let walk = open.walking.as_ref().expect("a walk began");
        assert_eq!(walk.stops.len(), 3, "declared, and assigned twice: {:?}", walk.stops);
        assert_eq!(walk.stops[1].1, "assigned");
        assert_eq!(walk.stops[2].1, "assigned");
        let (second, third) = (walk.stops[1].0.line, walk.stops[2].0.line);
        assert!(second < third, "in source order: {second} then {third}");

        app.pick_net(&mut open, net);
        assert_eq!(open.source_span().line, second, "the second click is the first assignment");
        app.pick_net(&mut open, net);
        assert_eq!(open.source_span().line, third, "the third is the other one");
        app.pick_net(&mut open, net);
        assert_eq!(open.source_span().line, declared, "and the fourth wraps to the declaration");
        app.open = Some(open);
    }

    /// A click on a different wire is a new question, not the next step of the
    /// old one.
    #[test]
    fn clicking_a_different_wire_starts_its_own_walk() {
        let mut app = opened("trace_demo.sv");
        let (staged, gate, gate_line) = {
            let open = app.open.as_ref().expect("a design");
            let module = open.design.module(open.current());
            let of = |want: &str| {
                module.nets.indices().find(|id| module.net(*id).name == want).expect("a net")
            };
            let gate = of("gate");
            (of("staged"), gate, module.net(gate).span.line)
        };

        let mut open = app.open.take().expect("a design");
        app.pick_net(&mut open, staged);
        app.pick_net(&mut open, staged);
        app.pick_net(&mut open, gate);

        assert_eq!(open.source_span().line, gate_line, "`gate` starts at its declaration");
        assert_eq!(open.walking.as_ref().map(|walk| walk.at), Some(0));
        app.open = Some(open);
    }

    /// The same for a track selected in the waveform, which is the other half
    /// of the ask: look at a row, and the source is already on the line that
    /// declares it.
    #[test]
    fn selecting_a_track_points_the_source_at_the_wire_it_records() {
        let mut app = watched("trace_demo.sv", "waves/trace_demo.vcd");
        app.show(Tab::Lint);

        let (signal, line) = {
            let open = app.open.as_ref().expect("a design");
            let module = open.design.module(open.current());
            let net = module
                .nets
                .indices()
                .find(|id| module.net(*id).name == "staged")
                .expect("`staged` is a net of the top");
            let node = open
                .flat
                .nodes
                .iter()
                .position(|node| node.path.is_empty())
                .expect("the top is a node");
            let signal = open.flat.nodes[node].signal(net).expect("it has a signal");
            (signal, module.net(net).span.line)
        };

        app.follow_source(signal);

        let open = app.open.as_ref().expect("still open");
        assert_eq!(open.source_span().line, line);
        assert!(app.is_visible(Tab::Lint), "the report is not dragged out of view");
    }

    /// Selecting a track lights its wire in the diagram, and does not move the
    /// diagram to do it. Two views agreeing about what is selected is most of
    /// what makes them one tool.
    #[test]
    fn selecting_a_track_lights_its_wire_in_the_diagram() {
        let mut app = watched("trace_demo.sv", "waves/trace_demo.vcd");

        let (signal, net) = {
            let open = app.open.as_ref().expect("a design");
            let module = open.design.module(open.current());
            let net = module
                .nets
                .indices()
                .find(|id| module.net(*id).name == "staged")
                .expect("`staged` is a net of the top");
            let node = open
                .flat
                .nodes
                .iter()
                .position(|node| node.path.is_empty())
                .expect("the top is a node");
            (open.flat.nodes[node].signal(net).expect("it has a signal"), net)
        };

        let before = app.open.as_ref().expect("open").path.len();
        app.follow_source(signal);

        let open = app.open.as_ref().expect("still open");
        assert!(open.highlighted.contains(&net), "the wire is lit: {:?}", open.highlighted);
        assert_eq!(open.path.len(), before, "and the diagram did not walk anywhere");
    }

    /// A wire in a module the diagram is not showing cannot be lit, and saying
    /// so beats a selection that appears to have done nothing.
    #[test]
    fn a_wire_in_another_module_says_where_it_is_rather_than_lighting_nothing() {
        let mut app = watched("trace_demo.sv", "waves/trace_demo.vcd");

        // `used` lives inside the child, and the diagram is at the top.
        let signal = {
            let open = app.open.as_ref().expect("a design");
            let node = open
                .flat
                .nodes
                .iter()
                .position(|node| !node.path.is_empty())
                .expect("the design has a child");
            let module = open.design.module(open.flat.nodes[node].module);
            let net = module
                .nets
                .indices()
                .find(|id| module.net(*id).name == "used")
                .expect("`used` is a net of the child");
            open.flat.nodes[node].signal(net).expect("it has a signal")
        };

        app.follow_source(signal);
        assert!(app.status.contains("inside"), "it says where: {}", app.status);
        assert!(app.status.contains("diagram"), "and how to get there: {}", app.status);
    }

    /// And the other way in: a name clicked in the source lights the same wire.
    #[test]
    fn a_name_clicked_in_the_source_lights_it_in_the_diagram_too() {
        let mut app = watched("trace_demo.sv", "waves/trace_demo.vcd");
        let net = {
            let open = app.open.as_ref().expect("a design");
            let module = open.design.module(open.current());
            module
                .nets
                .indices()
                .find(|id| module.net(*id).name == "gate")
                .expect("`gate` is a net of the top")
        };

        app.pick_named("gate");

        let open = app.open.as_ref().expect("still open");
        assert!(open.highlighted.contains(&net), "{:?}", open.highlighted);
    }

    /// The depth strip, asked without a pointer. Two fields and a button is a
    /// gesture nothing outside the window can make, so the knob is the only way
    /// this is ever checked.
    #[test]
    fn the_depth_strip_counts_the_clocks_between_two_signals() {
        let mut app = opened("pipeline3.sv");
        app.measure_depth("in_data,out_data");

        let open = app.open.as_ref().expect("a design");
        let report = open.depth.report.as_ref().expect("an answer");
        assert_eq!(report.min_stages, Some(3), "{report:#?}");
        assert_eq!(report.max_stages, Some(3));
        assert!(app.is_visible(Tab::Pipeline), "and it is in view");
        assert!(app.status.contains('3'), "the status says the number: {}", app.status);
    }

    /// A road that loops has a floor and no ceiling, and the pane has to carry
    /// that rather than a number with a caveat somewhere else.
    #[test]
    fn a_road_that_loops_is_shown_as_a_floor() {
        let mut app = opened("counter.sv");
        app.measure_depth("en,count");

        let open = app.open.as_ref().expect("a design");
        let report = open.depth.report.as_ref().expect("an answer");
        assert!(report.min_stages.is_some());
        assert_eq!(report.max_stages, None, "{report:#?}");
        assert!(report.feedback);
        assert!(app.status.contains("at least"), "{}", app.status);
    }

    /// A name that is not there says so rather than leaving the last answer on
    /// screen looking like this one.
    #[test]
    fn a_name_that_is_not_there_replaces_the_answer_rather_than_leaving_it() {
        let mut app = opened("pipeline3.sv");
        app.measure_depth("in_data,out_data");
        assert!(app.open.as_ref().unwrap().depth.report.as_ref().unwrap().min_stages.is_some());

        app.measure_depth("in_data,nonsense");
        let report = app.open.as_ref().unwrap().depth.report.as_ref().expect("still an answer");
        assert!(report.failed(), "the new question refused: {report:#?}");
        assert_eq!(report.min_stages, None, "and the old number is gone");
    }

    /// A word that is not a net has to say so. Silently doing nothing would
    /// leave a reader clicking a keyword and wondering what they missed.
    #[test]
    fn a_word_that_is_not_a_net_says_so_rather_than_doing_nothing() {
        let mut app = watched("trace_demo.sv", "waves/trace_demo.vcd");
        let before = app.wave.as_ref().map(|wave| wave.tracks.len()).expect("a dump is open");

        app.pick_named("always_ff");

        assert!(app.status.contains("not a net"), "it said why: {}", app.status);
        let after = app.wave.as_ref().map(|wave| wave.tracks.len()).expect("still open");
        assert_eq!(after, before, "and added nothing");
    }

    /// Only the names of the module the reader is actually looking at are
    /// offered. A file holds several modules, and `used` belongs to the child.
    #[test]
    fn the_names_offered_are_the_ones_in_the_module_on_screen() {
        let app = opened("trace_demo.sv");
        let open = app.open.as_ref().expect("a design");

        let names = RtlScopeApp::nets_in_view(open);
        assert!(names.contains("staged"), "a net of the top: {names:?}");
        assert!(names.contains("gate"), "and another: {names:?}");
        assert!(!names.contains("used"), "`used` is the child\'s: {names:?}");
        assert!(!names.contains("always_ff"), "a keyword is not a net: {names:?}");
    }

    /// The other direction of a click on a wire.
    ///
    /// A track that cannot say which net it records leaves the reader to find
    /// it by name — which is the work the matching already did once, and would
    /// get wrong for a module instantiated twice.
    #[test]
    fn a_track_leads_back_to_the_net_it_records() {
        let mut app = watched("pipeline3.sv", "waves/pipeline3.vcd");
        let signal = {
            let wave = app.wave.as_mut().expect("a dump");
            assert_eq!(wave.add_by_ir_name("data_d1"), crate::wave::Added::New, "{}", wave.status);
            match wave.tracks.last().expect("the track just added") {
                crate::wave::Track::Signal { signal, .. } => {
                    signal.expect("the design has this one")
                }
                _ => panic!("the track just added is a signal track"),
            }
        };

        app.show_signal(signal, Reveal::Trace);
        assert!(app.is_visible(Tab::Trace));
        assert_eq!(trail_of(&app), ["data_d1"], "{}", app.status);

        app.show_signal(signal, Reveal::Diagram);
        let open = app.open.as_ref().expect("open");
        let lit: Vec<&str> = open
            .highlighted
            .iter()
            .map(|net| open.design.module(open.current()).net(*net).name.as_str())
            .collect();
        assert_eq!(lit, ["data_d1"], "the wire is lit where the reader can see it");

        app.show_signal(signal, Reveal::Source);
        // Beside the trail rather than in its place: the source is in view
        // without the trail having gone anywhere.
        assert!(app.is_visible(Tab::Trace));
        assert!(app.is_visible(Tab::Source), "the source is in view beside it");
        let open = app.open.as_ref().expect("open");
        let (net, _) = open
            .design
            .module(open.current())
            .net_by_name("data_d1")
            .expect("pipeline3 declares it");
        let span = open.design.module(open.current()).net(net).span;
        assert_eq!(open.source_target, Some(span), "the line it is declared on");
    }

    /// The fixture recording, with one output changed from a moment on.
    ///
    /// Derived rather than committed: what the comparison has to be measured
    /// against is a second recording differing from the first in exactly one
    /// known way, and making it here means the two cannot drift apart.
    fn corrupted(from: &Path, name: &str) -> PathBuf {
        let text = std::fs::read_to_string(from).expect("the fixture recording");
        let mut out = String::new();
        let mut at = 0u64;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix('#')
                && let Ok(time) = rest.trim().parse()
            {
                at = time;
            }
            // `%` is out_data in this dump. Changed only once the pipeline is
            // full, so that the first difference is a moment worth naming
            // rather than tick zero.
            if at >= 180 && line.starts_with('b') && line.ends_with(" %") {
                let (bits, symbol) = line.split_once(' ').expect("`b<bits> <symbol>`");
                let flipped = if bits.ends_with('1') { '0' } else { '1' };
                out.push_str(&format!("{}{flipped} {symbol}\n", &bits[..bits.len() - 1]));
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        let path = std::env::temp_dir().join(format!("rtlscope-{name}.vcd"));
        std::fs::write(&path, out).expect("writable temp dir");
        path
    }

    /// Two recordings, and the one moment they part.
    ///
    /// The whole road: the comparison finds it, the panel marks the row it is
    /// on, and `first difference` puts the reader there — on the track it
    /// happened on, which it has to add, since the answer is no use without the
    /// row it is about.
    #[test]
    fn a_second_recording_leads_to_the_moment_the_two_part() {
        let good = rtlscope_fixtures::dir().join("waves/pipeline3.vcd");
        let bad = corrupted(&good, "compare");

        let mut app = watched("pipeline3.sv", "waves/pipeline3.vcd");
        app.compare_against(bad.clone());

        {
            let wave = app.wave.as_ref().expect("a dump");
            let reference = wave.reference.as_ref().expect("a second recording: {}");
            let report = &reference.comparison;
            assert!(report.problems.is_empty(), "{:?}", report.problems);
            assert!(report.only_in_a.is_empty() && report.only_in_b.is_empty());
            assert_eq!(report.differing.len(), 1, "{:?}", report.differing);

            let first = report.first().expect("they part");
            assert_eq!(first.path, "tb.dut.out_data");
            assert_eq!(first.at, 180, "the first moment, not the first change");
            assert!(reference.differs("tb.dut.out_data"), "the row is badged");
            assert!(!reference.differs("tb.dut.clk"), "and only that row");
        }

        app.seek_first_difference();

        let wave = app.wave.as_ref().expect("a dump");
        assert_eq!(wave.cursor, Some(180), "{}", app.status);
        let selected = wave.selected().expect("the row it happened on");
        let named = matches!(
            &wave.tracks[selected],
            crate::wave::Track::Signal { name, .. } if name == "tb.dut.out_data"
        );
        assert!(named, "the track was added and selected, not merely sought");

        let _ = std::fs::remove_file(&bad);
    }

    /// A dump holds the testbench's own signals too, and they are worth
    /// looking at even though the design does not declare them.
    #[test]
    fn a_variable_the_design_lacks_can_still_be_given_a_track() {
        let mut app = watched("pipeline3.sv", "waves/pipeline3.vcd");
        let wave = app.wave.as_mut().expect("a dump");

        assert!(wave.add_by_dump_path("tb.dut.clk").shown(), "{}", wave.status);
        let known =
            matches!(wave.tracks.last(), Some(crate::wave::Track::Signal { signal: Some(_), .. }));
        assert!(known, "the design has `clk`, so the track knows which wire it is");
    }

    /// A design copied somewhere writable, so a test can edit it.
    /// A directory of its own per test, not just a file of its own.
    ///
    /// Anything RTLScope writes beside a design — a saved drawing, for one — is
    /// named after the *module*, so two tests copying the same fixture into one
    /// directory would fight over that name however carefully their own file
    /// was named.
    fn scratch_of(fixture: &str, name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rtlscope-gui-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a writable temp dir");
        let path = dir.join(fixture);
        std::fs::copy(rtlscope_fixtures::path(fixture), &path).expect("the fixture copies");
        path
    }

    /// Clicking a wire is also asking where it comes from.
    #[test]
    fn a_clicked_wire_starts_a_trail() {
        let mut app = opened("fsm.sv");
        let mut open = app.open.take().expect("open");
        let net = net_of(&open.design, open.current(), "next_state");

        app.pick_net(&mut open, net);
        app.open = Some(open);

        assert_eq!(trail_of(&app), ["next_state"]);
        // No dump was opened, so there is no track — and the window says so
        // rather than looking like nothing happened. The fact, not the wording:
        // the sentence also carries where the source view went now, and pinning
        // the whole of it would break on every improvement to the phrasing.
        assert!(app.status.contains("no dump"), "{}", app.status);
    }

    /// Following a dependency is a step forward; a crumb is a step back.
    #[test]
    fn following_a_dependency_and_going_back_are_inverses() {
        let mut app = opened("fsm.sv");
        let mut open = app.open.take().expect("open");
        let module = open.current();
        let (next_state, state) =
            (net_of(&open.design, module, "next_state"), net_of(&open.design, module, "state"));
        app.pick_net(&mut open, next_state);
        app.open = Some(open);

        app.act(ViewAction::Trace(TraceStep::Follow(state)), Tab::Trace);
        assert_eq!(trail_of(&app), ["next_state", "state"]);
        assert!(app.is_visible(Tab::Trace));

        // Asking for the same hop twice does not lengthen the trail.
        app.act(ViewAction::Trace(TraceStep::Follow(state)), Tab::Trace);
        assert_eq!(trail_of(&app), ["next_state", "state"]);

        app.act(ViewAction::Trace(TraceStep::Back(1)), Tab::Trace);
        assert_eq!(trail_of(&app), ["next_state"]);
    }

    /// The two hops that cross an instance boundary, and that they undo each
    /// other.
    ///
    /// `hier_top.alu_y` is `u_alu.y`; inside, `y` is `a + b`; and `a` is a port
    /// the parent ties to `rf_rdata`. Walking in and back out has to land on
    /// that net of that module, not on something that merely shares its index.
    #[test]
    fn a_trail_crosses_an_instance_boundary_both_ways() {
        let mut app = opened("hier.sv");
        let mut open = app.open.take().expect("open");
        let top = open.current();
        let alu_y = net_of(&open.design, top, "alu_y");
        app.pick_net(&mut open, alu_y);
        app.open = Some(open);

        let step = TraceStep::Enter { instance: "u_alu".into(), port: "y".into() };
        app.act(ViewAction::Trace(step), Tab::Trace);
        assert_eq!(trail_of(&app), ["alu_y", "u_alu.y"], "{}", app.status);

        let inside = {
            let open = app.open.as_ref().expect("open");
            let hop = open.trail.last().expect("a hop");
            net_of(&open.design, hop.module(), "a")
        };
        app.act(ViewAction::Trace(TraceStep::Follow(inside)), Tab::Trace);
        assert_eq!(trail_of(&app), ["alu_y", "u_alu.y", "u_alu.a"]);

        app.act(ViewAction::Trace(TraceStep::Out { port: "a".into() }), Tab::Trace);
        assert_eq!(
            trail_of(&app),
            ["alu_y", "u_alu.y", "u_alu.a", "rf_rdata"],
            "out through `a` lands on what the parent connects to it"
        );
    }

    /// The edge of the design is a real answer, not a dead button.
    #[test]
    fn going_up_from_the_top_says_why_it_cannot() {
        let mut app = opened("fsm.sv");
        let mut open = app.open.take().expect("open");
        let clk = net_of(&open.design, open.current(), "clk");
        app.pick_net(&mut open, clk);
        app.open = Some(open);

        app.act(ViewAction::Trace(TraceStep::Out { port: "clk".into() }), Tab::Trace);

        assert_eq!(trail_of(&app), ["clk"], "the trail did not move");
        assert!(app.status.contains("outside the design"), "{}", app.status);
    }

    #[test]
    fn a_dropped_file_is_taken_for_what_its_name_says() {
        assert_eq!(of("top.sv"), Dropped::Source);
        assert_eq!(of("legacy.v"), Dropped::Source);
        assert_eq!(of("defs.svh"), Dropped::Source);
        assert_eq!(of("zybo.f"), Dropped::FileList);
        assert_eq!(of("src/top.veryl"), Dropped::Source, "Veryl is a source");
        assert_eq!(of("Veryl.toml"), Dropped::Source, "and so is the project that names it");
        assert_eq!(of("run.vcd"), Dropped::Dump);
        assert_eq!(of("sim/top.fst"), Dropped::Dump);
        assert_eq!(of("results.xml"), Dropped::Results);
    }

    /// Windows keeps whatever case the file was saved with, and a design saved
    /// from a Windows tool is as likely to be `TOP.SV` as `top.sv`.
    #[test]
    fn the_case_of_the_extension_does_not_matter() {
        assert_eq!(of("TOP.SV"), Dropped::Source);
        assert_eq!(of("Run.FST"), Dropped::Dump);
        assert_eq!(of("RESULTS.XML"), Dropped::Results);
    }

    /// Guessing would turn one true sentence into a page of syntax errors.
    #[test]
    fn anything_else_is_not_guessed_at() {
        assert_eq!(of("notes.txt"), Dropped::Unknown);
        assert_eq!(of("design.zip"), Dropped::Unknown);
        assert_eq!(of("Makefile"), Dropped::Unknown);
        assert_eq!(of(""), Dropped::Unknown);
    }

    /// The launcher's list is the eight views that report on a design, and
    /// every one of them says what it is for. A row with an empty line is a
    /// row that tells the reader nothing the tab strip did not.
    #[test]
    fn every_tool_the_launcher_lists_says_what_it_is_for() {
        assert_eq!(Tab::TOOLS.len(), 8);
        for tab in Tab::REPORTS {
            assert!(Tab::TOOLS.contains(&tab), "{} is a report and belongs here", tab.key());
        }
        assert!(Tab::TOOLS.contains(&Tab::Wave), "and the waveform, which reads a run");
        for tab in Tab::TOOLS {
            assert!(!what_it_answers(tab).is_empty(), "{} says nothing for itself", tab.key());
        }
        // The three that show the design rather than report on it are not in
        // the menu: they are the window, and they are always there.
        for tab in [Tab::Hierarchy, Tab::Diagram, Tab::Source] {
            assert!(!Tab::TOOLS.contains(&tab), "{} is not a tool", tab.key());
        }
        // Everything the menu launches is a view the desk knows how to place,
        // so a tool named here can always be brought up.
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        for tab in Tab::TOOLS {
            app.show(tab);
            assert!(app.is_visible(tab), "{} comes up when the menu asks", tab.key());
        }
    }

    /// The window simulates with Verilator until somebody says otherwise, and
    /// the saying is a setting rather than a fallback.
    #[test]
    fn the_window_simulates_with_verilator_until_it_is_told_otherwise() {
        let app = RtlScopeApp::new(ParseOptions::default(), None, None);
        assert_eq!(app.engine, rtlscope_tb::Engine::Verilator);
        assert_eq!(app.engine.name(), "verilator", "the name a settings file keeps");
        assert_eq!(rtlscope_tb::Engine::Icarus.name(), "icarus", "and the other one");
    }

    /// The third thing a window started from a shortcut cannot be told any
    /// other way. Before this box, `--editor` reached a terminal and nothing
    /// else, so an installed reader was stuck with the default editor.
    #[test]
    fn the_editor_comes_from_the_file_when_no_flag_named_one() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        assert_eq!(app.editor_command, DEFAULT_EDITOR, "until somebody says otherwise");
        assert!(!app.editor_pinned);

        let saved = crate::layout::Settings {
            editor: Some("subl {file}:{line}".to_string()),
            ..crate::layout::Settings::default()
        };
        app.take_settings(saved);
        assert_eq!(app.editor_command, "subl {file}:{line}");
    }

    /// A flag is one occasion, and must not quietly become the reader's
    /// setting — nor be overruled by one for the run it was typed for.
    #[test]
    fn a_flag_wins_the_run_it_was_given_for_and_leaves_the_file_alone() {
        let mut app =
            RtlScopeApp::new(ParseOptions::default(), None, Some("vim {file}".to_string()));
        assert!(app.editor_pinned);

        let saved = crate::layout::Settings {
            editor: Some("subl {file}:{line}".to_string()),
            ..crate::layout::Settings::default()
        };
        app.take_settings(saved);
        assert_eq!(app.editor_command, "vim {file}", "the flag holds for this run");
    }

    /// An editor lives in a directory with a space in it as often as not, and
    /// the file being opened may too. Splitting the whole line after the path
    /// was substituted tore both apart.
    #[test]
    fn a_command_keeps_its_quoted_parts_whole() {
        assert_eq!(
            split_command("code -g {file}:{line}:{col}"),
            ["code", "-g", "{file}:{line}:{col}"]
        );
        assert_eq!(
            split_command(r#""C:\Program Files\Editor\ed.exe" --at {file}"#),
            [r"C:\Program Files\Editor\ed.exe", "--at", "{file}"]
        );
        assert!(split_command("   ").is_empty(), "nothing to run is not an empty program");
    }

    /// The substitution happens per argument, so a path with a space stays one
    /// argument rather than becoming two.
    #[test]
    fn a_source_path_with_a_space_is_still_one_argument() {
        let parts: Vec<String> = split_command("code -g {file}:{line}")
            .into_iter()
            .map(|part| part.replace("{file}", r"C:\My Designs	op.sv").replace("{line}", "12"))
            .collect();
        assert_eq!(parts, ["code", "-g", r"C:\My Designs	op.sv:12"]);
    }

    /// The two boxes under `settings` are the only way in for a window opened
    /// by double-clicking a file: it inherits no shell's PATH and stands
    /// nowhere near a checkout, so neither the simulator nor a `.venv-cocotb`
    /// is anywhere it would look. What is typed has to reach the run.
    #[test]
    fn the_tool_paths_typed_in_settings_reach_the_run() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        assert_eq!(app.tools(), rtlscope_tb::Tools::default(), "empty asks for the old search");

        // Quoted and padded, which is how `Copy as path` in Explorer hands one
        // over and therefore how one arrives in the box.
        app.sim_dir = "\"E:/msys/ucrt64/bin\"".to_string();
        app.python = "  E:/work/.venv-cocotb/bin/python.exe  ".to_string();

        let tools = app.tools();
        assert_eq!(tools.sim_dir, Some(PathBuf::from("E:/msys/ucrt64/bin")));
        assert_eq!(tools.python, Some(PathBuf::from("E:/work/.venv-cocotb/bin/python.exe")));
        assert!(tools.near.is_empty(), "the design anchors it later, where it is known");
    }

    /// `reset layout` has to actually put every view back. A view left in a
    /// window with nothing to draw it, or missing from the desk altogether, is
    /// a view the reader cannot reach from anywhere.
    #[test]
    fn resetting_puts_every_view_back_on_the_main_surface() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.detach(Tab::Trace);
        app.detach(Tab::Lint);
        {
            let dock = app.dock.as_ref().expect("a desk");
            let floating = dock.iter_all_tabs().filter(|(path, _)| !path.surface.is_main()).count();
            assert_eq!(floating, 2, "two views in windows of their own");
        }

        app.forget_layout();

        let dock = app.dock.as_ref().expect("still a desk");
        assert!(
            dock.iter_all_tabs().all(|(path, _)| path.surface.is_main()),
            "no view is still off in a window",
        );
        assert!(
            dock.iter_surfaces().all(|surface| !matches!(surface, egui_dock::Surface::Window(..))),
            "and no window is left with nothing in it",
        );
        assert!(app.is_visible(Tab::Diagnostics), "and the desk has something it can show");
        assert!(app.has(Tab::Trace) && app.has(Tab::Lint), "with both views back among the rest");
    }

    /// A copy of the Veryl fixture somewhere disposable: opening it may run
    /// `veryl build`, and a test must not build inside the repository.
    fn veryl_project_copy(name: &str) -> PathBuf {
        fn copy_tree(from: &Path, to: &Path) {
            std::fs::create_dir_all(to).unwrap();
            for entry in std::fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                let target = to.join(entry.file_name());
                match entry.path().is_dir() {
                    true => copy_tree(&entry.path(), &target),
                    false => {
                        std::fs::copy(entry.path(), target).unwrap();
                    }
                }
            }
        }
        let dir = std::env::temp_dir().join(format!("rtlscope-gui-veryl-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        copy_tree(&rtlscope_fixtures::veryl_project(), &dir);
        dir
    }

    /// A Veryl project dropped as a folder opens as the SystemVerilog Veryl
    /// wrote for it, and every location still points into the `.veryl`.
    #[test]
    fn a_veryl_project_opens_with_its_locations_in_the_veryl() {
        let dir = veryl_project_copy("folder");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![dir.clone()], Bring::Replace);

        let open = app.open.as_ref().unwrap_or_else(|| panic!("{}", app.status));
        assert_eq!(open.design.modules.len(), 4, "{}", app.status);
        let top = open.design.module(open.design.top);
        assert_eq!(top.name, "lights_Top", "the project's prefix, as Veryl wrote it");
        assert_eq!(top.shown(), "Top", "and the name the author wrote, for the reader");
        let machines = rtlscope_analyse::fsm::find(&open.design);
        let control = machines.iter().find(|m| m.module_name == "Control").expect("the machine");
        let states: Vec<&str> = control.states.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(states, ["Idle", "Run", "Pause", "Done"], "states as written, not as generated");
        let placed = open.design.files.path(top.span.file).expect("a file");
        assert!(placed.ends_with("top.veryl"), "{}", placed.display());
        assert!(app.status.contains("Veryl") && app.status.contains("lights"), "{}", app.status);

        assert!(
            open.source_paths.iter().any(|p| p.ends_with("Veryl.toml")),
            "named by its manifest"
        );
        assert_eq!(open.sources_read.len(), 6, "{:?}", open.sources_read);
        assert!(open.sources_read.iter().all(|p| p.extension().is_some_and(|e| e == "sv")));
        assert!(open.watch_paths.iter().any(|p| p.ends_with("Veryl.toml")));
        assert_eq!(open.design.generated.len(), 6, "each .sv paired with its .veryl");
        let veryl = open.watch_paths.iter().filter(|p| p.extension().is_some_and(|e| e == "veryl"));
        assert_eq!(
            veryl.count(),
            6,
            "the sources are watched, not the output: {:?}",
            open.watch_paths
        );
    }

    /// The generated file can be looked at: a location asked for in it is
    /// honoured as it is, not pulled back to the `.veryl` it came from. That
    /// is what the Source view's `show control.sv` button relies on.
    #[test]
    fn a_location_in_the_generated_file_is_shown_as_the_generated_file() {
        let dir = veryl_project_copy("generated");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![dir.clone()], Bring::Replace);
        let open = app.open.as_ref().unwrap_or_else(|| panic!("{}", app.status));
        let pair = open
            .design
            .generated
            .iter()
            .find(|pair| {
                open.design.files.path(pair.original).is_some_and(|p| p.ends_with("control.veryl"))
            })
            .expect("control.sv is paired with control.veryl")
            .clone();
        let there = rtlscope_sv::generated_position(&pair.map, 3).expect("line 3 is mapped");
        let span = Span::new(pair.generated, there.0, there.1, 1);

        app.act(ViewAction::ShowSource(span), Tab::Fsm);

        let open = app.open.as_ref().expect("still open");
        let showing = open.design.files.path(open.source_span().file).expect("a file");
        assert!(showing.ends_with("control.sv"), "{}", showing.display());
        assert_eq!(
            crate::source::Language::of(Some(showing)),
            crate::source::Language::SystemVerilog,
            "and it is coloured as what it is"
        );
    }

    /// One `.veryl` file names the whole project it belongs to.
    #[test]
    fn a_single_veryl_file_opens_its_whole_project() {
        let dir = veryl_project_copy("file");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![dir.join("src").join("timer.veryl")], Bring::Replace);
        let open = app.open.as_ref().unwrap_or_else(|| panic!("{}", app.status));
        assert_eq!(open.design.modules.len(), 4, "not the one file: {}", app.status);
    }

    /// Somewhere to write a sample that is not the reader's own settings.
    fn sample_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rtlscope-gui-samples-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A sample opens the way a drop of the same files would: the design,
    /// its recording beside it, and the view its question is answered in.
    #[test]
    fn a_sample_opens_with_its_recording_and_its_view() {
        let home = sample_home("trace");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_sample_from("trace_demo", &home);
        assert!(app.open.is_some(), "the design opened: {}", app.status);
        assert!(app.wave.is_some(), "and the recording beside it: {}", app.status);
        assert!(app.is_visible(Tab::Trace), "on the view its question is answered in");
        assert!(
            home.join("trace_demo").join("trace_demo.sv").is_file(),
            "written to disk, where an editor can reach it"
        );
        assert!(app.status.contains("trace_demo"), "and said where: {}", app.status);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The cpu sample ends where a dropped project folder does: on the
    /// question of which design was meant. Two things live in that folder and
    /// nothing instantiates either, so both are offered.
    #[test]
    fn the_cpu_sample_asks_which_top() {
        let home = sample_home("cpu");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_sample_from("cpu", &home);
        assert!(app.open.is_none(), "several designs, so none opened on its own");
        let failed = app.failed.as_ref().expect("and the reasons are kept");
        for wanted in ["cpu", "blink"] {
            assert!(
                failed.tops.iter().any(|top| top == wanted),
                "`{wanted}` is offered: {:?}",
                failed.tops
            );
        }
        // And the testbench beside them is in hand, not among them: it is what
        // `simulate` will run once a top is chosen, and it is not a third
        // answer to which top that is.
        let bench = app.testbench().expect("the sample's testbench is loaded");
        assert_eq!(bench.top, "cpu_tb", "{}", app.status);
        assert!(app.status.contains("cpu_tb"), "and the status says so: {}", app.status);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Every sample names a view the tab bar has, and every one opens.
    #[test]
    fn every_sample_names_a_view_that_exists_and_opens() {
        let home = sample_home("all");
        for sample in crate::samples::ALL {
            if let Some(tab) = sample.tab {
                assert!(Tab::of(tab).is_some(), "{}: no view called {tab}", sample.id);
            }
            let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
            app.open_sample_from(sample.id, &home);
            match sample.id {
                "cpu" => assert!(app.failed.is_some(), "cpu: {}", app.status),
                _ => assert!(app.open.is_some(), "{} reads as a design: {}", sample.id, app.status),
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A top pinned for one design must not follow the reader to the next.
    #[test]
    fn a_top_pinned_for_one_design_does_not_stop_the_next() {
        let home = sample_home("pinned");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_sample_from("latch", &home);
        assert_eq!(app.top.as_deref(), Some("latch_check"), "the sample pinned its top");
        app.load(&[rtlscope_fixtures::path("hier.sv")], false);
        assert!(app.open.is_some(), "the next design still opens: {}", app.status);
        assert!(app.top.is_none(), "and the stale pin is gone");
        assert!(app.status.contains("latch_check"), "which was said: {}", app.status);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A top that is there stays pinned, as it always has.
    #[test]
    fn a_top_that_is_present_stays_pinned() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), Some("latch_check".into()), None);
        app.load(&[rtlscope_fixtures::path("latch.sv")], false);
        assert!(app.open.is_some(), "{}", app.status);
        assert_eq!(app.top.as_deref(), Some("latch_check"));
    }

    /// Files chosen in a dialog go through the same sorting as a drop, so the
    /// recording is read against the source whichever order they were picked.
    #[test]
    fn files_chosen_in_a_dialog_are_sorted_the_way_a_drop_is() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(
            vec![
                rtlscope_fixtures::wave("trace_demo.vcd"),
                rtlscope_fixtures::path("trace_demo.sv"),
            ],
            Bring::Replace,
        );
        assert!(app.open.is_some(), "the source opened first: {}", app.status);
        assert!(app.wave.is_some(), "and the recording was read against it");
    }

    /// A name clicked in the source is traced, the same as its wire in the
    /// diagram would be — so a Trace view beside the source answers for it,
    /// and nothing switches away from where the reader is.
    #[test]
    fn a_name_clicked_in_the_source_is_traced() {
        let mut app = opened("fsm.sv");
        app.show(Tab::Trace);
        app.pick_named("state");
        assert_eq!(trail_of(&app), ["state"], "{}", app.status);
        assert!(app.is_visible(Tab::Trace), "still where the reader was");
        assert!(app.status.contains("traced"), "and said so: {}", app.status);
        assert!(app.status.contains("no dump"), "and why there is no track: {}", app.status);
    }

    /// From any other view the trail is still set — quietly, ready for when
    /// the reader looks at Trace, without taking the view they are on away.
    #[test]
    fn a_name_clicked_in_the_source_does_not_change_the_view() {
        let mut app = opened("fsm.sv");
        app.show(Tab::Fsm);
        app.pick_named("next_state");
        assert_eq!(trail_of(&app), ["next_state"], "{}", app.status);
        assert!(app.is_visible(Tab::Fsm));
        assert_eq!(app.open.as_ref().unwrap().source_target, None, "and the source stayed put");
    }

    /// A sample with a bus to show puts it on the waveform itself, so the
    /// first handshake is on screen when the window opens.
    #[test]
    fn a_sample_puts_its_bus_on_the_waveform() {
        let home = sample_home("axi4");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_sample_from("axi4", &home);
        assert!(app.open.is_some(), "{}", app.status);
        let sample = crate::samples::find("axi4").expect("is a sample");
        let wave = app.wave.as_ref().expect("its recording opened");
        assert!(
            wave.tracks.len() >= sample.watch.len(),
            "{} track(s) for {} name(s): {}",
            wave.tracks.len(),
            sample.watch.len(),
            app.status
        );
        // A name inside an instance is found under it, not skipped as a name
        // the top does not have.
        let names: Vec<&str> = wave
            .tracks
            .iter()
            .filter_map(|track| match track {
                crate::wave::Track::Signal { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            names.iter().any(|name| name.ends_with("u_master.state")),
            "the master's state is on the panel: {names:?}"
        );
        assert!(
            app.status.contains("axi4_demo"),
            "the status is still about the design: {}",
            app.status
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Somewhere in the open design to point the source at.
    fn top_span(app: &RtlScopeApp) -> Span {
        let open = app.open.as_ref().expect("open");
        open.design.module(open.current()).span
    }

    /// A location clicked in a view brings the source up without taking that
    /// view away, so a state machine and the `case` it was read from can be
    /// looked at together. The default desk keeps them side by side.
    #[test]
    fn a_location_shown_from_a_view_keeps_that_view_on_screen() {
        let mut app = opened("fsm.sv");
        app.show(Tab::Fsm);
        let span = top_span(&app);
        app.act(ViewAction::ShowSource(span), Tab::Fsm);
        assert!(app.is_visible(Tab::Fsm), "the view stays");
        assert!(app.is_visible(Tab::Source), "and the source is beside it");
        assert_eq!(app.open.as_ref().unwrap().source_target, Some(span), "pointed at the line");
    }

    /// With the two in one group there is only one thing the panel can show,
    /// and a location asked for is the reader saying which.
    #[test]
    fn a_location_switches_to_the_source_when_the_two_share_a_group() {
        let mut app = opened("fsm.sv");
        app.dock = Some(egui_dock::DockState::new(vec![Tab::Fsm, Tab::Source]));
        app.show(Tab::Fsm);
        assert!(!app.is_visible(Tab::Source), "behind the machine to start with");

        let span = top_span(&app);
        app.act(ViewAction::ShowSource(span), Tab::Fsm);
        assert!(app.is_visible(Tab::Source), "and in front of it after");
        assert!(!app.is_visible(Tab::Fsm), "there being room for only one");
    }

    /// With the source in a window of its own, a location brings that window
    /// forward and leaves the view that asked exactly where it was.
    #[test]
    fn a_location_brings_a_floating_source_forward() {
        let mut app = opened("fsm.sv");
        app.show(Tab::Fsm);
        app.detach(Tab::Source);
        let source = app.dock.as_ref().unwrap().find_tab(&Tab::Source).expect("on the desk");
        assert!(!source.surface.is_main(), "floating");

        let span = top_span(&app);
        app.act(ViewAction::ShowSource(span), Tab::Fsm);

        assert!(app.is_visible(Tab::Fsm), "the view that asked keeps its place");
        assert!(app.is_visible(Tab::Source), "and the window is in front");
        let dock = app.dock.as_ref().unwrap();
        assert_eq!(
            dock.focused_leaf().map(|path| path.surface),
            Some(source.surface),
            "asked forward rather than merely still open",
        );
        assert_eq!(app.open.as_ref().unwrap().source_target, Some(span));
    }

    /// Pointing is softer than showing: the source follows the reader's eye
    /// only where they can already see it, and never takes a view away.
    #[test]
    fn pointing_moves_the_source_only_when_it_is_in_view() {
        // Behind the machine, so it is on the desk but not in sight.
        let mut app = opened("fsm.sv");
        app.dock = Some(egui_dock::DockState::new(vec![Tab::Fsm, Tab::Source]));
        app.show(Tab::Fsm);
        let span = top_span(&app);

        app.act(ViewAction::PointAt(span), Tab::Fsm);
        assert!(app.is_visible(Tab::Fsm), "nothing switched");
        assert_eq!(app.open.as_ref().unwrap().source_target, None, "and nothing moved");

        // The default desk, where the two are side by side.
        let mut app = opened("fsm.sv");
        app.show(Tab::Fsm);
        app.act(ViewAction::PointAt(span), Tab::Fsm);
        assert!(app.is_visible(Tab::Fsm), "still nothing switched");
        assert_eq!(app.open.as_ref().unwrap().source_target, Some(span), "in view, so it followed");
    }

    /// A view asking this while it is itself being drawn finds the dock out of
    /// the application's hands, and has to read the frame's own snapshot
    /// instead. Reading the missing dock as "nothing is visible" would make
    /// every location clicked in a view quietly fail to move the source.
    #[test]
    fn pointing_from_inside_a_view_reads_what_the_desk_last_showed() {
        let mut app = opened("fsm.sv");
        app.show(Tab::Fsm);
        let span = top_span(&app);

        // Exactly what `desk` does before handing the dock to the drawing.
        let dock = app.dock.take().expect("a desk");
        app.visible = crate::dock::visible(&dock);

        app.act(ViewAction::PointAt(span), Tab::Fsm);
        assert_eq!(
            app.open.as_ref().unwrap().source_target,
            Some(span),
            "the source was in view when the frame started, so it followed",
        );

        app.dock = Some(dock);
    }

    /// A move asked for while the dock is out is kept, not dropped.
    #[test]
    fn a_move_asked_for_while_the_desk_is_out_is_made_when_it_is_back() {
        let mut app = opened("fsm.sv");
        let dock = app.dock.take().expect("a desk");

        app.show(Tab::Trace);
        assert_eq!(app.dock_requests, vec![crate::dock::DockRequest::Show(Tab::Trace)]);

        app.dock = Some(dock);
        app.apply_dock_requests();
        assert!(app.is_visible(Tab::Trace), "and made as soon as the desk is back");
        assert!(app.dock_requests.is_empty(), "once only");
    }

    /// A simulation comes back to whichever view asked for it, and has to
    /// remember that even when the run itself is refused.
    #[test]
    fn a_simulation_remembers_the_view_that_asked() {
        let mut app = opened("fsm.sv");
        // Nothing to build from, so `start_run` turns back early.
        app.open.as_mut().expect("a design").sources_read.clear();

        app.act(ViewAction::Simulate, Tab::Pipeline);
        assert!(app.sim.is_none(), "refused, as this design cannot be simulated");
        assert_eq!(app.sim_from, Tab::Pipeline, "but it remembers who asked");
    }

    /// A testbench is a second attribute of the session, not a second design.
    /// Reading one leaves every view showing what it showed; what changes is
    /// what `simulate` will run.
    #[test]
    fn a_testbench_is_read_beside_the_design_and_not_into_it() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![rtlscope_fixtures::path("counter.sv")], Bring::Replace);
        let design = app.open.as_ref().expect("open").design.modules.len();
        let top = app.open.as_ref().expect("open").design.top_module().name.clone();

        app.open_testbench(vec![rtlscope_fixtures::path("counter_tb.sv")]);

        let bench = app.testbench().expect("a testbench");
        assert_eq!(bench.top, "counter_tb", "the module with no ports");
        assert!(bench.dumps, "the fixture records itself");
        let open = app.open.as_ref().expect("still open");
        assert_eq!(open.design.modules.len(), design, "the design did not change");
        assert_eq!(open.design.top_module().name, top, "and neither did its top");
        assert!(app.status.contains("counter_tb"), "and it said so: {}", app.status);
    }

    /// The testbench has a place in the hierarchy, under its own heading, and
    /// its row opens its source — from its own table, since its file is not in
    /// the design's. Pointing at the design again puts the Source tab back.
    #[test]
    fn the_testbench_row_reads_its_source_from_its_own_table() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![rtlscope_fixtures::path("counter.sv")], Bring::Replace);
        app.open_testbench(vec![rtlscope_fixtures::path("counter_tb.sv")]);
        let bench = app.testbench().expect("a testbench").clone();
        assert_eq!(bench.instances, [("dut".to_string(), "counter".to_string())]);

        // What clicking the row does.
        let open = app.open.as_mut().expect("open");
        open.show_bench_source(bench.span);
        assert!(open.source_in_bench, "the Source tab is looking at the testbench");
        let shown =
            bench.table.path(open.source_span().file).expect("resolves in the bench's table");
        assert!(shown.ends_with("counter_tb.sv"), "{}", shown.display());
        // And the design's table would have put the same id somewhere else,
        // which is why there are two.
        let in_design = open.design.files.path(open.source_span().file).expect("a design file");
        assert!(in_design.ends_with("counter.sv"), "{}", in_design.display());

        // What looking at the design does.
        let module = open.design.module(open.design.top).span;
        open.show_source(module);
        assert!(!open.source_in_bench, "back to the design");
    }

    /// Handing it a design is a mistake worth naming. Reading `counter.sv` as
    /// a testbench and running it would simulate a module with no stimulus at
    /// all, and the waveform would be flat with nothing to say why.
    #[test]
    fn a_design_offered_as_a_testbench_is_refused_and_the_old_one_kept() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![rtlscope_fixtures::path("counter.sv")], Bring::Replace);
        app.open_testbench(vec![rtlscope_fixtures::path("counter_tb.sv")]);

        app.open_testbench(vec![rtlscope_fixtures::path("counter.sv")]);

        assert_eq!(
            app.testbench().map(|bench| bench.top.as_str()),
            Some("counter_tb"),
            "the one that was working was replaced by one that does not: {}",
            app.status
        );
        assert!(app.status.contains("takes none"), "and the reason is named: {}", app.status);
    }

    /// Putting it down goes back to a generated harness, which is the other
    /// half of a toggle: a reader who tried a testbench must be able to stop.
    #[test]
    fn a_testbench_can_be_put_down_again() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![rtlscope_fixtures::path("counter.sv")], Bring::Replace);
        app.open_testbench(vec![rtlscope_fixtures::path("counter_tb.sv")]);
        assert!(app.testbench().is_some());

        app.drop_testbench();

        assert!(app.testbench().is_none());
        assert!(app.status.contains("generates a harness again"), "{}", app.status);
    }

    /// A module chosen in the tree becomes the design: the same read again
    /// with it pinned as the top, which is what `--top` does and what the
    /// welcome screen's "which one?" does when a read was ambiguous. From the
    /// tree it is the case where the reader is looking at the design and wants
    /// less of it.
    #[test]
    fn a_module_set_as_top_from_the_tree_becomes_the_design() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![rtlscope_fixtures::path("hier.sv")], Bring::Replace);
        assert_eq!(app.open.as_ref().expect("open").design.top_module().name, "hier_top");

        app.set_top("hier_ctrl".to_string());

        let open = app.open.as_ref().unwrap_or_else(|| panic!("{}", app.status));
        assert_eq!(open.design.top_module().name, "hier_ctrl", "{}", app.status);
        assert_eq!(app.top.as_deref(), Some("hier_ctrl"), "and the choice sticks");
        assert!(open.source_paths.iter().any(|p| p.ends_with("hier.sv")), "same sources");
    }

    /// Two files that are one design, which is the ordinary shape of real RTL
    /// and the thing the window could not open without naming both at once.
    fn split_design(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rtlscope-gui-add-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        std::fs::write(
            dir.join("top.sv"),
            "module add_top (\n\
             \x20   input  logic       clk,\n\
             \x20   input  logic [7:0] a,\n\
             \x20   output logic [7:0] y\n\
             );\n\
             \x20   add_leaf u_leaf (.clk(clk), .d(a), .q(y));\n\
             endmodule\n",
        )
        .expect("the top");
        std::fs::write(
            dir.join("leaf.sv"),
            "module add_leaf (\n\
             \x20   input  logic       clk,\n\
             \x20   input  logic [7:0] d,\n\
             \x20   output logic [7:0] q\n\
             );\n\
             \x20   always_ff @(posedge clk) q <= d;\n\
             endmodule\n",
        )
        .expect("the leaf");
        dir
    }

    /// Whether the top's one instance still has no source behind it.
    fn instance_is_a_blackbox(app: &RtlScopeApp) -> bool {
        let open = app.open.as_ref().unwrap_or_else(|| panic!("{}", app.status));
        let top = open.design.module(open.design.top);
        let inst = top.insts.first().expect("the top instantiates something");
        open.design.module(inst.of).is_blackbox
    }

    /// The feature: a design whose parts live in two files can be opened one
    /// file at a time. Before the add the instance is a hole the window draws
    /// as a black box; after it, it is the module it always was.
    #[test]
    fn a_source_added_joins_the_design_instead_of_replacing_it() {
        let dir = split_design("joins");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![dir.join("top.sv")], Bring::Replace);
        assert!(instance_is_a_blackbox(&app), "the leaf has no source yet: {}", app.status);

        app.open_paths(vec![dir.join("leaf.sv")], Bring::Add);
        assert!(!instance_is_a_blackbox(&app), "the leaf did not join: {}", app.status);
        let open = app.open.as_ref().expect("still open");
        assert_eq!(open.source_paths.len(), 2, "both files are what this design is read from");
        assert!(app.status.contains("added 1 file"), "and it said so: {}", app.status);
    }

    /// The rule that makes adding safe to try: files that will not read with
    /// what is open cost nothing. The design stays exactly as it was, and the
    /// reasons go where a broken edit's reasons go.
    ///
    /// Getting a read to fail at all takes doing, which is the front end
    /// working as intended: syntax it cannot make sense of is reported and
    /// skipped, not refused. What is left is a read with no module in it at
    /// all — here, the one file the design came from has been deleted since it
    /// was opened, and what is being added declares no module of its own.
    #[test]
    fn files_that_will_not_join_leave_the_open_design_alone() {
        let dir = std::env::temp_dir().join("rtlscope-gui-add-refused");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let only = dir.join("only.sv");
        std::fs::write(&only, "module add_only (input logic c);\nendmodule\n").expect("the file");

        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![only.clone()], Bring::Replace);
        let before = app.open.as_ref().expect("open").design.modules.len();

        std::fs::remove_file(&only).expect("it goes away underneath");
        let header = dir.join("defs.sv");
        std::fs::write(&header, "localparam int ADD_W = 8;\n").expect("the header");
        app.open_paths(vec![header], Bring::Add);

        let open =
            app.open.as_ref().unwrap_or_else(|| panic!("the design was lost: {}", app.status));
        assert_eq!(open.design.modules.len(), before, "the design changed: {}", app.status);
        assert_eq!(open.source_paths.len(), 1, "and it is still read from the one file");
        let stale = open.stale.as_ref().expect("the reasons are kept");
        assert!(stale.from_add, "and are labelled as an add rather than an edit");
        assert!(stale.diags.count(Severity::Error) > 0, "with something in them");
        assert!(app.status.contains("nothing was added"), "{}", app.status);
    }

    /// Adding a file whose module nothing instantiates gives the read a second
    /// design to choose between, and that is the only way an ordinary add
    /// fails. It is not a question the reader asked — they are adding to the
    /// design in front of them — so it is answered with the top they are
    /// already looking at, and said out loud, because nothing in the window
    /// will look any different afterwards.
    #[test]
    fn adding_a_module_nothing_instantiates_keeps_the_top_that_is_open() {
        let dir = split_design("second-top");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![dir.join("top.sv"), dir.join("leaf.sv")], Bring::Replace);
        let top_was = app.open.as_ref().expect("open").design.top_module().name.clone();

        let other = dir.join("other.sv");
        std::fs::write(
            &other,
            "module add_other (input logic c, output logic q);\n  assign q = c;\nendmodule\n",
        )
        .expect("the other module");
        app.open_paths(vec![other], Bring::Add);

        let open = app.open.as_ref().unwrap_or_else(|| panic!("{}", app.status));
        assert!(open.stale.is_none(), "the add was refused: {}", app.status);
        assert_eq!(open.design.top_module().name, top_was, "the top moved: {}", app.status);
        assert_eq!(open.source_paths.len(), 3, "and the file did join the list");
        assert!(app.status.contains("is still the top"), "and it said so: {}", app.status);
    }

    /// A file already in the design is not read a second time. Reading one
    /// twice declares its modules twice, so the reader would be handed a pile
    /// of redefinitions for choosing a file that was already there.
    #[test]
    fn a_file_already_in_the_design_is_not_added_again() {
        let dir = split_design("twice");
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![dir.join("top.sv"), dir.join("leaf.sv")], Bring::Replace);
        let before = app.open.as_ref().expect("open").design.modules.len();

        app.open_paths(vec![dir.join("leaf.sv")], Bring::Add);
        let open = app.open.as_ref().unwrap_or_else(|| panic!("{}", app.status));
        assert_eq!(open.design.modules.len(), before, "something was read twice: {}", app.status);
        assert_eq!(open.source_paths.len(), 2, "and the list did not grow");
        assert!(open.stale.is_none(), "nothing failed: {}", app.status);
        assert!(app.status.contains("already in this design"), "{}", app.status);
    }

    /// Adding with nothing open is opening: there is nothing to join, and
    /// refusing would be answering a reasonable act with a rule.
    #[test]
    fn adding_with_nothing_open_simply_opens() {
        let mut app = RtlScopeApp::new(ParseOptions::default(), None, None);
        app.open_paths(vec![rtlscope_fixtures::path("counter.sv")], Bring::Add);
        assert!(app.open.is_some(), "{}", app.status);
    }
}
