//! The analysis tabs: what the CLI prints, readable in the window.
//!
//! Nothing here analyses anything — each view draws a report the
//! `rtlscope-analyse` crate produced, the same one `rtlscope fsm` or `rtlscope cdc`
//! prints, so the window and the terminal cannot disagree. What a view adds is
//! what a terminal cannot: a click that goes to the source line, a machine
//! picked from a list, a colour that says "unsynchronised" before the word
//! does.
//!
//! Every location is a link. The analyses exist to point at code, and a report
//! that names a line the reader then has to go and find by hand is half a
//! report.

use std::collections::BTreeSet;

use egui::{Align2, Color32, FontId, Layout, RichText, ScrollArea, Ui, Vec2};
use rtlscope_analyse::pipeline::{PipelineReport, Stage};
use rtlscope_analyse::{Crossing, CrossingKind, DomainReport, Fsm, LintReport, fsm::ANY_OTHER};
use rtlscope_graph::state::StateGeom;
use rtlscope_ir::{Design, Diagnostics, FileTable, ModuleId, NetId, Severity, Span};
use rtlscope_wave::flow::TokenFlow;
use rtlscope_wave::stages::Cell;

use crate::states;
use crate::theme::{Theme, badge};
use crate::tree::{self, Crumb};

/// What a view asked the application to do.
pub enum ViewAction {
    /// Show this location in the Source tab. Every `file:line` in every view
    /// means this now; leaving the application is a separate, explicit ask.
    ShowSource(Span),
    /// Point the source at this location without going there.
    ///
    /// Taken up only when the source is already in view — beside the panel's
    /// view, or in a window of its own — and otherwise dropped. This is what
    /// selecting a state means: the reader is looking at the machine, and the
    /// `case` arm should follow their hand without taking the machine away.
    PointAt(Span),
    /// Hand this location to the external editor.
    OpenEditor(Span),
    /// Count the clocks between the two signals the depth pane names.
    ///
    /// No payload: the pane holds what was typed, and passing it through the
    /// action as well would make two places able to disagree about what is
    /// being asked.
    MeasureDepth,
    /// A name clicked in the source, to be treated as its wire in the diagram
    /// would be: traced, lit, and put on the waveform when one is open.
    ///
    /// A name rather than a `SignalId` because the source view is text: it has
    /// a file and some lines, and no idea which net any of it is. Resolving is
    /// the application's job, which is also where the instance path lives —
    /// and the path is half the answer, since one net of a module instantiated
    /// twice is two signals.
    PickNamed(String),
    /// Show this module's diagram.
    Goto(ModuleId),
    /// Lay this clock domain out cycle by cycle in the wave panel.
    OpenStages(String),
    /// Put the waveform's cursor on this cycle. Clicking a cell in the flow
    /// view is the same gesture as clicking that moment in the waveform.
    SeekCycle(usize),
    /// Follow the tokens through a different window of cycles.
    FlowWindow(usize),
    /// Simulate the design and open what comes out.
    Simulate,
    /// Put the selected machine's state register on the waveform.
    ///
    /// No payload, for the reason [`ViewAction::MeasureDepth`] has none: which
    /// machine is selected is the pane's, and passing it through here as well
    /// would make two places able to disagree about what was asked.
    WatchState,
    /// Move the cursor to when the selected state is next entered.
    ///
    /// Likewise without a payload: the state is the one the reader clicked, and
    /// the pane is already holding it.
    SeekNextEntry,
    /// A step of a provenance walk. Its own enum because the five gestures
    /// only mean anything together, and none of them means anything to a view
    /// that is not the trace.
    Trace(TraceStep),
}

/// One move in "where does this value come from".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceStep {
    /// Follow a signal the current driver depends on, in the same module.
    Follow(NetId),
    /// Give a signal a waveform track, without leaving the trail.
    Watch(NetId),
    /// Into the child instance whose output this is.
    Enter { instance: String, port: String },
    /// Out through this module's input, to whatever the parent connects to it.
    Out { port: String },
    /// Back to an earlier hop. The number is how many hops to keep, so the
    /// first crumb is `Back(1)`.
    Back(usize),
}

/// Which way the pipeline is being looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PipeMode {
    /// The chain of stages: what the design *is*.
    #[default]
    Structure,
    /// One row per beat, one column per cycle: what a run *did*.
    Flow,
}

/// A `file:line` that goes somewhere when clicked.
///
/// Only the file's own name is shown — a full path would eat the row — and the
/// whole path is on hover for when two files share a name.
fn location(ui: &mut Ui, files: &FileTable, span: Span) -> bool {
    let full = files.render(span);
    let short = full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string();
    ui.link(RichText::new(short).monospace().small().weak()).on_hover_text(full).clicked()
}

/// A heading for a section inside a tab: small caps, muted, with a count.
fn section(ui: &mut Ui, title: &str, count: usize) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title.to_uppercase()).small().weak());
        ui.label(RichText::new(count.to_string()).small().monospace().weak());
    });
}

// ------------------------------------------------------------ diagnostics ---

/// Everything the front end could not read, worst first.
pub fn diagnostics(ui: &mut Ui, files: &FileTable, diags: &Diagnostics) -> Option<ViewAction> {
    let theme = Theme::of(ui);
    let mut action = None;

    let errors = diags.count(Severity::Error);
    let warnings = diags.count(Severity::Warning);
    ui.horizontal(|ui| {
        badge(ui, &format!("{errors} error(s)"), theme.err, theme.err_soft);
        badge(ui, &format!("{warnings} warning(s)"), theme.warn, theme.warn_soft);
        if diags.is_empty() {
            ui.label(RichText::new("the whole design was read").weak());
        }
    });
    ui.add_space(2.0);

    // Collected first: a row can ask to open the editor, which needs the app.
    let rows: Vec<(Severity, &'static str, String, Option<Span>)> = diags
        .iter()
        .map(|diag| (diag.severity, diag.code.id(), diag.message.clone(), diag.span))
        .collect();

    ScrollArea::vertical().id_salt("diagnostics").auto_shrink(false).show(ui, |ui| {
        for (severity, code, message, span) in &rows {
            let (strong, soft) = match severity {
                Severity::Error => (theme.err, theme.err_soft),
                Severity::Warning => (theme.warn, theme.warn_soft),
                Severity::Info => (theme.muted, theme.surface_alt),
            };
            ui.horizontal_wrapped(|ui| {
                badge(ui, code, strong, soft);
                ui.label(message);
                if let Some(span) = span
                    && location(ui, files, *span)
                {
                    action = Some(ViewAction::ShowSource(*span));
                }
            });
        }
    });
    action
}

// -------------------------------------------------------------------- fsm ---

/// Which way a machine is being looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FsmMode {
    /// States and arrows: the shape of the machine, which is the one thing no
    /// list can show.
    #[default]
    Diagram,
    /// Every transition with its guard, in the order the source wrote them.
    List,
}

/// What the FSM tab remembers between frames.
///
/// The layout is kept here rather than recomputed each frame, and — more to the
/// point — so is the scene's rect: panning and zooming a diagram only works if
/// where the reader put it survives the next frame.
/// What the depth strip is asking, and what came back.
///
/// A pane rather than four arguments, for the reason [`FsmPane`] is one: the
/// question survives a redraw and the answer belongs beside it, and a view with
/// eleven parameters is one nobody can add a tenth to safely.
#[derive(Default)]
pub struct DepthPane {
    /// The two signals, as typed. Names, not resolved signals: the resolving
    /// is the application's, and it says in the status when it had to guess.
    pub from: String,
    pub to: String,
    /// The structural answer, once asked for.
    pub report: Option<rtlscope_analyse::DepthReport>,
    /// And the measured one, when a recording was open to measure over.
    pub measured: Option<rtlscope_wave::LatencyReport>,
    /// What the two say about each other.
    pub cross: Vec<String>,
}

/// Fresh, a pane shows the first machine as a diagram, unplaced — so the first
/// frame fits it to the panel.
#[derive(Default)]
pub struct FsmPane {
    pub selected: usize,
    pub mode: FsmMode,
    /// The state the reader last clicked, whose transitions light up.
    ///
    /// Public because it is also which state a `next ▸` is asking about, and
    /// answering that means reading the dump — which is the application's job
    /// and not this view's.
    pub state: Option<usize>,
    /// Which machine `geom` belongs to.
    laid_out: Option<usize>,
    geom: StateGeom,
    /// Where the diagram has been put: a scale and an offset, the same as the
    /// cone keeps, so a pane dragged taller shows more of the same drawing.
    placement: crate::canvas::Placement,
}

impl FsmPane {
    /// A pane reading a machine the way one before it did.
    ///
    /// The layout is deliberately not carried across: it belongs to a design
    /// that has been read again, and laying it out afresh costs less than
    /// deciding whether the old one is still true.
    pub fn with_mode(mode: FsmMode) -> Self {
        Self { mode, ..Self::default() }
    }

    /// Lays a machine out once, when it becomes the one being looked at.
    fn ensure(&mut self, fsms: &[Fsm]) {
        if self.laid_out == Some(self.selected) {
            return;
        }
        self.geom = rtlscope_graph::state_diagram(&fsms[self.selected]);
        self.laid_out = Some(self.selected);
        self.state = None;
        // A different machine is a different picture, so it is fitted afresh.
        self.placement.refit();
    }
}

/// The state machines: what each one is made of, and the shape it makes.
/// What the open recording says the machine is doing, at the cursor.
///
/// Read by the application and handed in, the way [`Occupancy`] is: this view
/// has a machine and no waveform, the wave panel has a waveform and no machine,
/// and only the thing holding both can put one to the other.
///
/// `None` is every kind of not-knowing at once — no recording, no cursor, no
/// such register in it — because all of them come to the same thing here: draw
/// the machine plain.
pub struct Now {
    /// Where the cursor is, in the dump's own ticks.
    pub at: u64,
    /// Which of the machine's states the register held there.
    ///
    /// `None` when it held a value no state is named for. Worth saying rather
    /// than passing over: an encoding outside the machine's own set is the
    /// thing a `default:` arm exists to catch, and a reader looking at one
    /// wants to be told that is what they are looking at.
    pub state: Option<usize>,
    /// What it held, when that was a number at all — `x` before the first
    /// clock is not.
    pub value: Option<u64>,
    /// Which instance it was read from, when the module has more than one.
    pub instance: Option<String>,
}

pub fn fsm(
    ui: &mut Ui,
    files: &FileTable,
    fsms: &[Fsm],
    pane: &mut FsmPane,
    now: Option<Now>,
) -> Option<ViewAction> {
    let theme = Theme::of(ui);
    let mut action = None;

    if fsms.is_empty() {
        empty_state(
            ui,
            "no state machines",
            "A state machine here means a register whose next value a `case` on itself\n\
             decides. A design that encodes state some other way will not show up.",
        );
        return None;
    }
    pane.selected = pane.selected.min(fsms.len() - 1);
    pane.ensure(fsms);

    ui.horizontal_top(|ui| {
        // ---- the list ----
        ui.allocate_ui_with_layout(
            Vec2::new(250.0, ui.available_height()),
            Layout::top_down_justified(egui::Align::LEFT),
            |ui| {
                ScrollArea::vertical().id_salt("fsm-list").auto_shrink(false).show(ui, |ui| {
                    for (index, machine) in fsms.iter().enumerate() {
                        let label = format!(
                            "{}.{}  ({})",
                            machine.module_name,
                            machine.state_name,
                            machine.states.len()
                        );
                        if ui
                            .selectable_label(
                                index == pane.selected,
                                RichText::new(label).monospace(),
                            )
                            .clicked()
                        {
                            pane.selected = index;
                        }
                    }
                });
            },
        );
        ui.separator();

        // ---- the machine ----
        let machine = &fsms[pane.selected];
        ui.vertical(|ui| {
            // Wrapped, because this row has grown past what a narrow pane can
            // hold: name, clock, counts, where it was written, its alarms, and
            // what the recording says. A plain `horizontal` clips the end off,
            // and the end is where the reading is — measured in a dock with the
            // machine in a side pane, which is exactly where somebody watching
            // a waveform would put it.
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{}.{}", machine.module_name, machine.state_name))
                        .monospace()
                        .strong(),
                );
                ui.label(RichText::new(format!("clock {}", machine.clock)).monospace().weak());
                ui.label(
                    RichText::new(format!(
                        "{} state(s), {} transition(s)",
                        machine.states.len(),
                        machine.transitions.len()
                    ))
                    .small()
                    .weak(),
                );
                // The machine's own alarms, counted here as well as coloured on
                // the boxes: a reader who has not looked closely still learns
                // there is something to look closely at.
                if !machine.unreachable.is_empty() {
                    ui.label(
                        RichText::new(format!("· {} unreachable", machine.unreachable.len()))
                            .small()
                            .color(theme.warn),
                    );
                }
                if !machine.terminal.is_empty() {
                    ui.label(
                        RichText::new(format!("· {} dead end", machine.terminal.len()))
                            .small()
                            .color(theme.err),
                    );
                }
                // Muted, and deliberately not `warn`. A machine with no
                // `default:` is not thereby wrong — a `case` covering every
                // encoding of its own width needs none — so this is a fact the
                // reader may want, not a fault they must answer for. The ones
                // that are faults are theirs to judge.
                if !machine.transitions.iter().any(|arm| arm.from == ANY_OTHER) {
                    ui.label(RichText::new("· no default arm").small().weak().color(theme.muted))
                        .on_hover_text(
                            "No `default:`, so an encoding none of the arms name leaves the \
                         machine where it is. Sound when the arms cover every encoding of \
                         the register's width, and a way to get stuck when they do not.",
                        );
                }
                if location(ui, files, machine.span) {
                    action = Some(ViewAction::ShowSource(machine.span));
                }

                // ---- what the recording says, when there is one ----
                if let Some(now) = &now {
                    ui.separator();
                    let (text, colour) = match now.state.and_then(|at| machine.states.get(at)) {
                        Some(state) => (format!("at {}: {}", now.at, state.name), theme.accent),
                        // A value the machine does not name is the `default:`
                        // arm's whole subject, so it is said as that rather
                        // than printed as a bare number.
                        None => match now.value {
                            Some(value) => (
                                format!("at {}: {value} — no state has this value", now.at),
                                theme.warn,
                            ),
                            None => (format!("at {}: not a number yet", now.at), theme.muted),
                        },
                    };
                    ui.label(RichText::new(text).small().monospace().color(colour));
                    if let Some(instance) = &now.instance {
                        ui.label(RichText::new(format!("in {instance}")).small().weak());
                    }
                    if ui
                        .button("watch")
                        .on_hover_text("Put this machine's state register on the waveform")
                        .clicked()
                    {
                        action = Some(ViewAction::WatchState);
                    }
                    if let Some(picked) = pane.state.and_then(|at| machine.states.get(at))
                        && ui
                            .button("next ▸")
                            .on_hover_text(format!(
                                "Move the cursor to when `{}` is next entered",
                                picked.name
                            ))
                            .clicked()
                    {
                        action = Some(ViewAction::SeekNextEntry);
                    }
                }

                ui.separator();
                ui.selectable_value(&mut pane.mode, FsmMode::Diagram, "diagram")
                    .on_hover_text("The states, and the arrows between them");
                ui.selectable_value(&mut pane.mode, FsmMode::List, "list")
                    .on_hover_text("Every transition with its guard, in source order");
                if ui
                    .button("block diagram")
                    .on_hover_text("Show the diagram of the module this machine lives in")
                    .clicked()
                {
                    action = Some(ViewAction::Goto(machine.module));
                }

                // Never silently wrong: an arm the picture has no place for
                // says so beside the picture, not nowhere.
                if !pane.geom.dropped.is_empty() {
                    badge(
                        ui,
                        &format!("{} not drawn", pane.geom.dropped.len()),
                        theme.warn,
                        theme.warn_soft,
                    )
                    .on_hover_text(format!(
                        "These arms lead to or from something the machine does not name as a \
                         state, so there is nothing to draw them between:\n{}",
                        pane.geom.dropped.join("\n")
                    ));
                }
            });

            match pane.mode {
                FsmMode::Diagram => {
                    // Destructured for the disjoint borrows: the placement is
                    // written while the geometry is read.
                    //
                    // Not inside an `egui::Scene`, as it once was. A `Scene`
                    // scales the whole layer, glyphs included, so a small
                    // machine fitted to a wide pane was drawn at 3× as a
                    // stretched picture of 11pt type — the state names came
                    // out smeared. `states::draw` owns its transform the way
                    // the block canvas does, and lays every label out at the
                    // size it is seen at.
                    let FsmPane { geom, placement, state, .. } = pane;
                    let lit = now.as_ref().and_then(|now| now.state);
                    let drawn = states::draw(ui, geom, *state, lit, placement);
                    if let Some(at) = drawn.selected {
                        // Clicking the same state again puts the highlight
                        // away, so the picture can be read plain once more.
                        *state = (*state != Some(at)).then_some(at);
                        // And the source follows the selection, where it is in
                        // view: a state is the `case` arm it was declared in.
                        if *state == Some(at)
                            && let Some(picked) = geom.states.get(at)
                            && !picked.span.is_unknown()
                        {
                            action = Some(ViewAction::PointAt(picked.span));
                        }
                    }
                    if let Some(span) = drawn.source
                        && !span.is_unknown()
                    {
                        action = Some(ViewAction::ShowSource(span));
                    }
                }
                FsmMode::List => list(ui, theme, machine),
            }
        });
    });
    action
}

/// The machine as text: every state badged with its fate, then every
/// transition. What a diagram cannot do is hold a guard too long to fit on an
/// arrow, and this is where those stay readable.
fn list(ui: &mut Ui, theme: &Theme, machine: &Fsm) {
    ui.horizontal_wrapped(|ui| {
        for state in &machine.states {
            let is_reset = machine.reset_state.as_deref() == Some(state.name.as_str());
            let unreachable = machine.unreachable.contains(&state.name);
            let terminal = machine.terminal.contains(&state.name);
            let (strong, soft) = if unreachable {
                (theme.warn, theme.warn_soft)
            } else if terminal {
                (theme.err, theme.err_soft)
            } else if is_reset {
                (theme.accent, theme.accent_soft)
            } else {
                (theme.muted, theme.surface_alt)
            };
            // Spelled out rather than a glyph: the house symbol is
            // not in egui's fonts, and colour alone should not carry it.
            let text =
                if is_reset { format!("{} · reset", state.name) } else { state.name.clone() };
            badge(ui, &text, strong, soft).on_hover_text(match (unreachable, terminal) {
                (true, _) => "never entered, other than through reset",
                (_, true) => "never left",
                _ if is_reset => "the reset state",
                _ => "a state",
            });
        }
    });

    ScrollArea::vertical().id_salt("fsm-detail").auto_shrink(false).show(ui, |ui| {
        for transition in &machine.transitions {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    // `->` rather than an arrow glyph. This label is monospace, and no
                    // face in that family on this machine has U+2192 — but every
                    // one of them has the two characters RTL would have written
                    // anyway.
                    RichText::new(format!("{} -> {}", transition.from, transition.to)).monospace(),
                );
                if !transition.guard.is_empty() {
                    ui.label(
                        RichText::new(format!("when {}", transition.guard.join(" && ")))
                            .monospace()
                            .weak(),
                    );
                }
            });
        }
    });
}

/// Says the picture is behind the sources, above the reasons why.
///
/// Drawn instead of a blank tab, because "these are the errors" without "and
/// the diagram is the previous version" would have the reader looking at one
/// design and reading about another.
pub fn stale_note(ui: &mut Ui, from_add: bool) {
    let theme = Theme::of(ui);
    // Two things bring a reader here and they are not the same thing. An edit
    // that stopped reading leaves a window showing something older than the
    // files; an add that would not join leaves it showing exactly what it
    // showed before, with nothing lost. Saying "the sources have moved on" to
    // somebody who just picked a file would be describing the wrong event.
    let (badge_text, sentence) = match from_add {
        true => (
            "nothing was added",
            "What is in the window is what was already open, unchanged. Below is why the \
             files that were chosen would not read with it.",
        ),
        false => (
            "the sources have moved on",
            "Everything else in the window is the last version that read as a design. \
             Below is what stopped the newest one.",
        ),
    };
    ui.horizontal_wrapped(|ui| {
        badge(ui, badge_text, theme.warn, theme.warn_soft);
        ui.label(RichText::new(sentence).small().weak());
    });
    ui.separator();
}

// -------------------------------------------------------------------- cdc ---

/// The clock domains, and what crosses between them.
pub fn cdc(ui: &mut Ui, files: &FileTable, report: &DomainReport) -> Option<ViewAction> {
    let theme = Theme::of(ui);
    let mut action = None;

    let handled = report.crossings.iter().filter(|c| c.kind.is_handled()).count();
    let bare = report.crossings.len() - handled;
    ui.horizontal(|ui| {
        badge(ui, &format!("{} domain(s)", report.domains.len()), theme.accent, theme.accent_soft);
        badge(ui, &format!("{handled} synchronised"), theme.ok, theme.ok_soft);
        badge(ui, &format!("{bare} not recognised"), theme.err, theme.err_soft);
        ui.label(
            RichText::new(format!("over {} clocked process(es)", report.flops)).small().weak(),
        );
    });
    ui.add_space(2.0);

    ui.horizontal_top(|ui| {
        // ---- the domains ----
        ui.allocate_ui_with_layout(
            Vec2::new(210.0, ui.available_height()),
            Layout::top_down_justified(egui::Align::LEFT),
            |ui| {
                ScrollArea::vertical().id_salt("cdc-domains").auto_shrink(false).show(ui, |ui| {
                    for domain in &report.domains {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&domain.clock).monospace());
                            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("{} flop(s)", domain.flops))
                                        .small()
                                        .weak(),
                                );
                            });
                        });
                    }
                });
            },
        );
        ui.separator();

        // ---- the crossings, worst first ----
        ui.vertical(|ui| {
            let mut ordered: Vec<&Crossing> = report.crossings.iter().collect();
            ordered.sort_by_key(|crossing| match crossing.kind {
                CrossingKind::MultiBitUnsynchronised => 0,
                CrossingKind::Unsynchronised => 1,
                CrossingKind::TwoFlopSynchroniser => 2,
            });
            ScrollArea::vertical().id_salt("cdc-crossings").auto_shrink(false).show(ui, |ui| {
                for crossing in ordered {
                    let (text, strong, soft) = match crossing.kind {
                        CrossingKind::TwoFlopSynchroniser => ("2-flop", theme.ok, theme.ok_soft),
                        CrossingKind::Unsynchronised => ("bare", theme.warn, theme.warn_soft),
                        CrossingKind::MultiBitUnsynchronised => {
                            ("multi-bit", theme.err, theme.err_soft)
                        }
                    };
                    ui.horizontal_wrapped(|ui| {
                        badge(ui, text, strong, soft);
                        ui.label(RichText::new(&crossing.signal).monospace());
                        ui.label(
                            RichText::new(format!("{} → {}", crossing.from, crossing.to))
                                .monospace()
                                .weak(),
                        );
                        if crossing.width > 1 {
                            ui.label(
                                RichText::new(format!("{} bits", crossing.width)).small().weak(),
                            );
                        }
                        if !crossing.at.is_empty() {
                            ui.label(RichText::new(format!("in {}", crossing.at)).small().weak());
                        }
                        if location(ui, files, crossing.span) {
                            action = Some(ViewAction::ShowSource(crossing.span));
                        }
                    });
                }
                if report.crossings.is_empty() {
                    ui.label(RichText::new("nothing crosses between the domains").weak());
                }
            });
        });
    });
    action
}

// ------------------------------------------------------------------- lint ---

/// Inferred latches, dead signals, and modules nothing instantiates.
pub fn lint(ui: &mut Ui, files: &FileTable, report: &LintReport) -> Option<ViewAction> {
    let theme = Theme::of(ui);
    let mut action = None;

    if report.is_empty() {
        empty_state(
            ui,
            "nothing to report",
            "No inferred latches, no dead signals, no uninstantiated modules.\n\
             Processes RTLScope could not model are in Diagnostics, not silently passed here.",
        );
        return None;
    }

    ScrollArea::vertical().id_salt("lint").auto_shrink(false).show(ui, |ui| {
        section(ui, "inferred latches", report.latches.len());
        for latch in &report.latches {
            ui.horizontal_wrapped(|ui| {
                badge(ui, "latch", theme.err, theme.err_soft);
                ui.label(RichText::new(format!("{}.{}", latch.module, latch.net)).monospace());
                ui.label(RichText::new(&latch.because).weak());
                if location(ui, files, latch.span) {
                    action = Some(ViewAction::ShowSource(latch.span));
                }
            });
        }
        if report.latches.is_empty() {
            ui.label(RichText::new("none").small().weak());
        }

        section(ui, "dead signals", report.dead_nets.len());
        for net in &report.dead_nets {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{}.{}", net.module, net.net)).monospace());
                ui.label(RichText::new(format!("{} bit(s), driven, never read", net.width)).weak());
                if location(ui, files, net.span) {
                    action = Some(ViewAction::ShowSource(net.span));
                }
            });
        }
        if report.dead_nets.is_empty() {
            ui.label(RichText::new("none").small().weak());
        }

        section(ui, "instantiated by nothing", report.dead_modules.len());
        for module in &report.dead_modules {
            ui.label(RichText::new(module).monospace());
        }
        if report.dead_modules.is_empty() {
            ui.label(RichText::new("none").small().weak());
        }
    });
    action
}

// ------------------------------------------------------------------ trace ---

/// One hop of a provenance walk: a net, and where in the hierarchy it lives.
///
/// The whole chain of crumbs rather than a bare module id, for two reasons a
/// module id alone cannot serve: following a signal out through an input needs
/// the way back out, and giving one a waveform track needs the instance path a
/// dump knows it by. A module instantiated five times has five answers to both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub path: Vec<Crumb>,
    pub net: NetId,
}

impl Hop {
    /// The module the net belongs to.
    pub fn module(&self) -> ModuleId {
        self.path.last().expect("a hop always carries at least the top crumb").module
    }
}

/// Where a signal's value comes from — one hop, with the way to take another.
///
/// The answer is [`rtlscope_analyse::drive::trace`], the same one the `drivers`
/// MCP tool gives; what the window adds is that every name in it is a place to
/// go next. One hop at a time is the point rather than a limitation: a reader
/// following provenance is deciding which dependency mattered, and a view that
/// unrolled the whole cone would have answered a question nobody asked.
/// The Trace tab's own controls: which question, and how far.
///
/// Above both views because they are two readings of one thing. Returns true
/// when the question changed, which is when the drawing has to be fitted again
/// rather than left under a camera aimed at a cone of another shape.
pub fn cone_controls(ui: &mut Ui, pane: &mut crate::app::ConePane) -> bool {
    use rtlscope_analyse::cone::Towards;

    let mut changed = false;
    ui.horizontal(|ui| {
        // Two readings of one signal: where exactly its value came from, and
        // what is around it at all.
        if ui.selectable_label(!pane.drawn, "provenance").clicked() {
            pane.drawn = false;
        }
        if ui
            .selectable_label(pane.drawn, "cone")
            .on_hover_text("What reaches this signal, or what it reaches, as a picture")
            .clicked()
        {
            pane.drawn = true;
        }
        if !pane.drawn {
            return;
        }

        ui.separator();
        for (towards, label, hint) in [
            (Towards::Drivers, "drivers", "What decides this signal"),
            (Towards::Loads, "loads", "What this signal decides"),
        ] {
            if ui
                .selectable_label(pane.towards == towards, label)
                .on_hover_text(hint)
                .clicked()
                && pane.towards != towards
            {
                pane.towards = towards;
                changed = true;
            }
        }

        ui.separator();
        ui.label(RichText::new("depth").small().weak());
        let was = pane.depth;
        ui.add(egui::DragValue::new(&mut pane.depth).range(1..=8).speed(0.1))
            .on_hover_text(
                "How many hops to follow. Three reaches through two registers and the                  logic between them.",
            );
        changed |= pane.depth != was;
    });
    ui.separator();
    changed
}

pub fn trace(
    ui: &mut Ui,
    design: &Design,
    flat: &rtlscope_analyse::flat::Flattened,
    trail: &[Hop],
) -> Option<ViewAction> {
    use rtlscope_analyse::drive::Kind;

    let theme = Theme::of(ui);
    let Some(here) = trail.last() else {
        empty_state(
            ui,
            "nothing traced yet",
            "Click a wire in the diagram, or a name in the source, to ask where its value \
             comes from.\n\
             Every name in the answer is somewhere else to go.",
        );
        return None;
    };
    let mut action = None;
    let files = &design.files;
    let traced = rtlscope_analyse::drive::trace(design, flat, here.module(), here.net);

    // The trail, newest last, and only once there is one: a single hop is
    // named again immediately below, and printing it twice says nothing.
    // `from` rather than an arrow, because the direction is the whole meaning.
    if trail.len() > 1 {
        ui.horizontal_wrapped(|ui| {
            for (index, hop) in trail.iter().enumerate() {
                if index > 0 {
                    ui.label(RichText::new("from").small().weak());
                }
                let name = design.module(hop.module()).net(hop.net).shown();
                let text = RichText::new(name).monospace();
                if index + 1 == trail.len() {
                    ui.label(text.strong());
                } else if ui.link(text).clicked() {
                    action = Some(ViewAction::Trace(TraceStep::Back(index + 1)));
                }
            }
        });
        ui.separator();
    }

    ScrollArea::vertical().id_salt("trace").auto_shrink(false).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&traced.name).monospace().strong());
            ui.label(RichText::new(format!("{} bit(s)", traced.width)).weak());
            let path = tree::instance_path(&here.path);
            if !path.is_empty() {
                ui.label(RichText::new(format!("in {path}")).monospace().weak());
            }
            if ui.small_button("wave").on_hover_text("give this signal a track").clicked() {
                action = Some(ViewAction::Trace(TraceStep::Watch(here.net)));
            }
        });

        section(ui, "driven by", traced.drivers.len());
        if traced.drivers.is_empty() {
            ui.label(RichText::new("nothing — see below").small().weak());
        }
        for (index, driver) in traced.drivers.iter().enumerate() {
            ui.horizontal_wrapped(|ui| {
                match &driver.kind {
                    Kind::Register { clock, reset } => {
                        badge(ui, "register", theme.accent, theme.accent_soft);
                        ui.label(RichText::new(format!("on {clock}")).monospace());
                        if let Some(reset) = reset {
                            ui.label(RichText::new(format!("reset {reset}")).monospace().weak());
                        }
                    }
                    Kind::Combinational => {
                        badge(ui, "combinational", theme.ok, theme.ok_soft);
                    }
                    Kind::Latch => {
                        badge(ui, "latch", theme.err, theme.err_soft);
                        ui.label(RichText::new("assigned on some paths only").weak());
                    }
                    Kind::Initial => {
                        badge(ui, "initial", theme.warn, theme.warn_soft);
                        ui.label(RichText::new("simulation only").weak());
                    }
                    Kind::FromOutside { port } => {
                        badge(ui, "from outside", theme.accent, theme.accent_soft);
                        ui.label(RichText::new(format!("port {port}")).monospace());
                        if ui
                            .small_button("go up")
                            .on_hover_text("what the parent connects to this port")
                            .clicked()
                        {
                            let step = TraceStep::Out { port: port.clone() };
                            action = Some(ViewAction::Trace(step));
                        }
                    }
                    Kind::FromInstance { instance, of, port } => {
                        badge(ui, "from an instance", theme.accent, theme.accent_soft);
                        ui.label(RichText::new(format!("{instance}.{port}")).monospace());
                        ui.label(RichText::new(of).weak());
                        if ui
                            .small_button("go inside")
                            .on_hover_text("what drives that port within the child")
                            .clicked()
                        {
                            let step =
                                TraceStep::Enter { instance: instance.clone(), port: port.clone() };
                            action = Some(ViewAction::Trace(step));
                        }
                    }
                    Kind::Nothing => {
                        badge(ui, "undriven", theme.err, theme.err_soft);
                        ui.label(RichText::new("nothing in the design puts a value on it").weak());
                    }
                }
                if location(ui, files, driver.span) {
                    action = Some(ViewAction::ShowSource(driver.span));
                }
            });

            if driver.depends.is_empty() {
                continue;
            }
            ui.indent(("depends", index), |ui| {
                for on in &driver.depends {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(&on.name).monospace());
                        // The condition it came in under, quoted, or the fact
                        // that it did not come in under one.
                        match &on.through {
                            Some(text) => ui.label(RichText::new(text).monospace().weak()),
                            None => ui.label(RichText::new("data").weak()),
                        };
                        if ui.small_button("follow").clicked() {
                            action = Some(ViewAction::Trace(TraceStep::Follow(on.net)));
                        }
                        if ui.small_button("wave").clicked() {
                            action = Some(ViewAction::Trace(TraceStep::Watch(on.net)));
                        }
                    });
                }
            });
        }

        // What the answer does not cover, said rather than left to be assumed.
        if !traced.problems.is_empty() {
            section(ui, "not accounted for", traced.problems.len());
            for problem in &traced.problems {
                ui.label(RichText::new(problem).weak());
            }
        }
    });
    action
}

// --------------------------------------------------------------- pipeline ---

/// What the open waveform says the pipeline was doing at the cursor.
///
/// Built from the stage view the wave panel already has, rather than computed
/// again here: the two pictures are then the same reading of the same dump,
/// and the diagram cannot disagree with the waveform it sits above.
pub struct Occupancy {
    /// The clock the wave panel laid out, so only that domain is coloured.
    pub clock: String,
    pub cycle: usize,
    /// Stage index, and what that stage held on that cycle.
    pub cells: Vec<(usize, Cell)>,
}

impl Occupancy {
    fn of(&self, stage: usize) -> Option<Cell> {
        self.cells.iter().find(|(index, _)| *index == stage).map(|(_, cell)| *cell)
    }
}

/// How wide one stage is, and how much room the arrow between two of them gets.
const STAGE_WIDTH: f32 = 172.0;
const STAGE_GAP: f32 = 28.0;

/// How many clocks deep the logic is, drawn.
///
/// A pipeline is a chain, so it is drawn as one: a card per stage, left to
/// right, with what lives at that depth written inside it. The indented list
/// this replaced was the same information and could not be read at a glance —
/// which is the only thing a reader wants from a pipeline, since its shape *is*
/// the answer.
///
/// With a dump open and its stages laid out, each card takes the colour of what
/// that stage held at the cursor. The structure comes from the IR and the state
/// comes from the run, which is the whole thesis of the tool in one picture.
#[allow(clippy::too_many_arguments)]
pub fn pipeline(
    ui: &mut Ui,
    report: &PipelineReport,
    selected: &mut usize,
    mode: &mut PipeMode,
    wave_open: bool,
    occupancy: Option<&Occupancy>,
    flow: Option<&TokenFlow>,
    cycles: usize,
    simulating: Option<&str>,
    refused: Option<&str>,
    cycles_wanted: &mut u64,
    range: std::ops::RangeInclusive<u64>,
    depth: &mut DepthPane,
    files: &FileTable,
) -> Option<ViewAction> {
    let theme = Theme::of(ui);
    let mut action = None;

    if report.domains.is_empty() {
        empty_state(
            ui,
            "nothing is clocked",
            "A stage is a position in the register adjacency graph. A design with no\n\
             registers in it has no depth to report.",
        );
        return None;
    }
    *selected = (*selected).min(report.domains.len() - 1);

    // ---- what there is, and which domain is being looked at ----
    ui.horizontal(|ui| {
        // Two readings of one pipeline: what it is, and what it did.
        if ui.selectable_label(*mode == PipeMode::Structure, "structure").clicked() {
            *mode = PipeMode::Structure;
        }
        if ui
            .selectable_label(*mode == PipeMode::Flow, "flow")
            .on_hover_text("Follow each beat through the pipe, a row per beat — needs a dump")
            .clicked()
        {
            *mode = PipeMode::Flow;
        }
        ui.separator();
        badge(ui, &format!("{} register(s)", report.registers), theme.accent, theme.accent_soft);
        if report.domains.len() > 1 {
            for (index, domain) in report.domains.iter().enumerate() {
                let label = format!("{}  ({})", domain.clock, domain.depth);
                if ui
                    .selectable_label(index == *selected, RichText::new(label).monospace())
                    .on_hover_text(format!("{} stage(s) deep", domain.depth))
                    .clicked()
                {
                    *selected = index;
                }
            }
        }
    });

    let domain = &report.domains[*selected];
    let lit = occupancy.filter(|shown| shown.clock == domain.clock);

    if *mode == PipeMode::Flow {
        return flow_view(
            ui,
            domain,
            wave_open,
            flow,
            lit.map(|shown| shown.cycle),
            cycles,
            simulating,
            refused,
            cycles_wanted,
            range,
        );
    }

    ui.horizontal(|ui| {
        ui.label(RichText::new(&domain.clock).monospace().strong());
        ui.label(RichText::new(format!("{} stage(s) deep", domain.depth)).weak());
        if wave_open
            && ui
                .button("cycles…")
                .on_hover_text("Lay these stages against the open dump, cycle by cycle")
                .clicked()
        {
            action = Some(ViewAction::OpenStages(domain.clock.clone()));
        }
        match lit {
            Some(shown) => {
                ui.label(
                    RichText::new(format!("showing cycle {}", shown.cycle))
                        .small()
                        .color(theme.accent),
                );
            }
            None if wave_open => {
                ui.label(
                    RichText::new("press cycles… and put the cursor somewhere to colour these")
                        .small()
                        .weak(),
                );
            }
            None => {}
        }
    });
    ui.add_space(4.0);

    // ---- two points on it ----
    // Above the chain rather than below: a reader who has just been told the
    // design is four deep is exactly the reader who wants to know how deep
    // *this* road is, and putting the question under the picture makes them
    // scroll past the answer to ask it.
    if let Some(asked) = depth_strip(ui, depth, files) {
        action = Some(asked);
    }
    ui.separator();

    // ---- the chain ----
    let height = (ui.available_height() - 10.0).max(96.0);
    ScrollArea::horizontal().id_salt("pipeline-chain").show(ui, |ui| {
        ui.horizontal_top(|ui| {
            for (position, stage) in domain.stages.iter().enumerate() {
                let feedback = domain.feedback.iter().any(|group| group.stage == stage.index);
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(STAGE_WIDTH, height), egui::Sense::hover());
                if ui.is_rect_visible(rect) {
                    card(ui, rect, stage, feedback, lit.and_then(|shown| shown.of(stage.index)));
                }
                // The names that did not fit are still an answer, on hover.
                if stage.registers.len() > 1 {
                    response.on_hover_text(stage.registers.join("\n"));
                }
                if position + 1 < domain.stages.len() {
                    let (gap, _) =
                        ui.allocate_exact_size(Vec2::new(STAGE_GAP, height), egui::Sense::hover());
                    arrow(ui.painter(), gap, theme.line);
                }
            }
        });
    });
    action
}

/// How wide one cycle is, and how tall one token's row is.
const CELL_WIDTH: f32 = 15.0;
const ROW_HEIGHT: f32 = 15.0;
/// How wide the token labels down the left are.
const LABEL_WIDTH: f32 = 96.0;

/// Every beat that went through the pipe, a row each, a cycle per column.
///
/// The transpose of the stage diagram, and the reading a person actually wants:
/// a beat's life is a diagonal, a stall is a flat run, and a bubble is the gap
/// between two diagonals. Colour is the *stage* a beat was in, so the diagonal
/// is a gradient marching down the palette and anything that breaks the pattern
/// breaks it visibly.
#[allow(clippy::too_many_arguments)]
fn flow_view(
    ui: &mut Ui,
    domain: &rtlscope_analyse::pipeline::DomainDepth,
    wave_open: bool,
    flow: Option<&TokenFlow>,
    cursor: Option<usize>,
    cycles: usize,
    simulating: Option<&str>,
    refused: Option<&str>,
    cycles_wanted: &mut u64,
    range: std::ops::RangeInclusive<u64>,
) -> Option<ViewAction> {
    let theme = Theme::of(ui);
    let mut action = None;

    if !wave_open {
        empty_state(
            ui,
            "no dump open",
            "Following a beat through the pipe means watching one run of it.\n\
             Drop a .vcd or .fst on the window — or make one from this design.",
        );
        ui.add_space(8.0);
        if simulate_button(ui, simulating, refused, cycles_wanted, range) {
            action = Some(ViewAction::Simulate);
        }
        return action;
    }
    let Some(flow) = flow.filter(|flow| flow.clock == domain.clock) else {
        ui.add_space(ui.available_height() * 0.25);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("the stages are not laid out yet").heading().weak());
            ui.add_space(6.0);
            if ui.button(format!("lay out `{}`", domain.clock)).clicked() {
                action = Some(ViewAction::OpenStages(domain.clock.clone()));
            }
        });
        return action;
    };

    // ---- what was followed, and what could not be ----
    ui.horizontal_wrapped(|ui| {
        badge(ui, &format!("{} beat(s)", flow.tokens.len()), theme.accent, theme.accent_soft);
        ui.label(
            RichText::new(format!("cycles {} .. {} of {cycles}", flow.first, flow.last()))
                .small()
                .weak(),
        );
        if flow.first > 0 && ui.button("◀ earlier").clicked() {
            action = Some(ViewAction::FlowWindow(flow.first.saturating_sub(flow.len)));
        }
        if flow.last() + 1 < cycles && ui.button("later ▶").clicked() {
            action = Some(ViewAction::FlowWindow(flow.first + flow.len));
        }
        let stalls: usize = flow.tokens.iter().map(rtlscope_wave::Token::stalls).sum();
        if stalls > 0 {
            badge(ui, &format!("{stalls} stalled cycle(s)"), theme.warn, theme.warn_soft);
        }
        // The rule cannot follow every pipeline, and a picture that hid where
        // it failed would be worse than no picture.
        for problem in &flow.problems {
            badge(ui, "cannot follow", theme.err, theme.err_soft).on_hover_text(problem);
        }
    });
    ui.add_space(2.0);

    if flow.is_empty() {
        empty_state(
            ui,
            "nothing went through",
            "No stage was carrying anything in these cycles. Try another window, or\n\
             check that the valid bit each stage was read from is the right one.",
        );
        return action;
    }

    // ---- the grid ----
    //
    // Culled against the scroll area's own viewport rather than against a clip
    // rectangle: `show_viewport` hands over the visible window in *content*
    // coordinates, which is the space the cells are laid out in, so there is no
    // conversion to get subtly wrong. A full window is 4096 columns by as many
    // rows, and painting all of it every frame would make this unusable.
    let rows = flow.tokens.len();
    let size =
        Vec2::new(LABEL_WIDTH + CELL_WIDTH * flow.len as f32, ROW_HEIGHT * (rows + 1) as f32);
    let dark = ui.visuals().dark_mode;

    let inner = ScrollArea::both().id_salt("flow-grid").auto_shrink(false).show_viewport(
        ui,
        |ui, viewport| {
            let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
            let painter = ui.painter_at(rect);
            let origin = rect.left_top();
            // Content space: x = 0 at the left of the labels, y = 0 at the ruler.
            let x_of = |cycle: usize| LABEL_WIDTH + CELL_WIDTH * (cycle - flow.first) as f32;
            let seen = |x: f32| x + CELL_WIDTH >= viewport.min.x && x <= viewport.max.x;

            // The ruler, every tenth cycle.
            for cycle in (flow.first..=flow.last()).filter(|c| c.is_multiple_of(10)) {
                let x = x_of(cycle);
                if !seen(x) {
                    continue;
                }
                painter.text(
                    origin + Vec2::new(x, 0.0),
                    Align2::LEFT_TOP,
                    cycle.to_string(),
                    FontId::proportional(9.5),
                    theme.muted,
                );
            }

            // Where the waveform's cursor is, so the two views agree at a glance.
            if let Some(at) = cursor.filter(|at| *at >= flow.first && *at <= flow.last()) {
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        origin + Vec2::new(x_of(at), 0.0),
                        Vec2::new(CELL_WIDTH, size.y),
                    ),
                    0.0,
                    theme.accent_soft,
                );
            }

            let pointer = response.hovered().then(|| ui.input(|i| i.pointer.hover_pos())).flatten();
            let mut hovered: Option<(usize, usize, usize, bool)> = None;

            let first_row = ((viewport.min.y / ROW_HEIGHT) as usize).saturating_sub(1);
            let last_row = ((viewport.max.y / ROW_HEIGHT) as usize + 1).min(rows);
            for row in first_row..last_row {
                let token = &flow.tokens[row];
                let top = ROW_HEIGHT * (row + 1) as f32;
                painter.text(
                    origin + Vec2::new(3.0, top + ROW_HEIGHT / 2.0),
                    Align2::LEFT_CENTER,
                    elide(&token.label, 13),
                    FontId::monospace(9.5),
                    if token.appeared_mid_pipe || token.vanished { theme.err } else { theme.muted },
                );

                for step in &token.steps {
                    let x = x_of(step.cycle);
                    if !seen(x) {
                        continue;
                    }
                    let cell = egui::Rect::from_min_size(
                        origin + Vec2::new(x, top + 1.0),
                        Vec2::new(CELL_WIDTH - 1.0, ROW_HEIGHT - 2.0),
                    );
                    let colour = if step.unknown {
                        theme.err
                    } else {
                        crate::theme::stage_colour(step.stage, dark)
                    };
                    // A stall is the same stage again: drawn recessed and
                    // hollow, so a held run reads as a pause rather than as
                    // more progress.
                    if step.stalled {
                        painter.rect_filled(cell, 2.0, colour.gamma_multiply(0.3));
                        painter.rect_stroke(
                            cell,
                            2.0,
                            egui::Stroke::new(1.0, colour),
                            egui::StrokeKind::Inside,
                        );
                    } else {
                        painter.rect_filled(cell, 2.0, colour);
                    }
                    painter.text(
                        cell.center(),
                        Align2::CENTER_CENTER,
                        step.stage.to_string(),
                        FontId::proportional(9.0),
                        theme.ink,
                    );
                    if pointer.is_some_and(|at| cell.contains(at)) {
                        hovered = Some((row, step.cycle, step.stage, step.stalled));
                    }
                }
            }

            if let Some((row, cycle, stage, stalled)) = hovered {
                let token = &flow.tokens[row];
                let mut lines = vec![token.label.clone(), format!("cycle {cycle} · stage {stage}")];
                if stalled {
                    lines.push("stalled — the pipe did not advance".into());
                }
                if token.appeared_mid_pipe {
                    lines.push("appeared with nothing above it to have come from".into());
                }
                if token.vanished {
                    lines.push("stopped before the last stage".into());
                }
                response.clone().on_hover_text(lines.join("\n"));
            }

            // Clicking a cell is clicking that moment: the cursor, the waveform
            // and the structure diagram all move together.
            if response.clicked()
                && let Some(at) = response.interact_pointer_pos()
            {
                let x = at.x - origin.x;
                if x >= LABEL_WIDTH {
                    let cycle = flow.first + ((x - LABEL_WIDTH) / CELL_WIDTH) as usize;
                    if cycle <= flow.last() {
                        return Some(ViewAction::SeekCycle(cycle));
                    }
                }
            }
            None
        },
    );
    inner.inner.or(action)
}

/// One stage: what is at that depth, and what it was doing.
fn card(ui: &mut Ui, rect: egui::Rect, stage: &Stage, feedback: bool, cell: Option<Cell>) {
    let theme = Theme::of(ui);
    let painter = ui.painter();

    // Colour is the run's answer, never the structure's. "Read, and the stage
    // was empty" and "nothing is known about this cycle" must not look alike:
    // the first is a fact and the second is the absence of one, so an idle
    // stage takes the recessed surface and an unread one keeps the plain card.
    let (fill, edge) = match cell {
        Some(Cell::Busy) => (theme.ok_soft, theme.ok),
        Some(Cell::Held) => (theme.warn_soft, theme.warn),
        Some(Cell::Unknown) => (theme.err_soft, theme.err),
        Some(Cell::Idle) => (theme.surface_alt, theme.line),
        Some(Cell::Blank) | None => (theme.surface, theme.line),
    };
    painter.rect_filled(rect, egui::CornerRadius::same(5), fill);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(5),
        egui::Stroke::new(1.0, edge),
        egui::StrokeKind::Inside,
    );

    let inner = rect.shrink2(Vec2::new(9.0, 7.0));
    painter.text(
        inner.left_top(),
        Align2::LEFT_TOP,
        format!("stage {}", stage.index),
        FontId::proportional(11.5),
        theme.ink,
    );
    painter.text(
        inner.right_top(),
        Align2::RIGHT_TOP,
        match cell {
            Some(Cell::Busy) => "carrying".to_string(),
            Some(Cell::Held) => "stalled".to_string(),
            Some(Cell::Unknown) => "undriven".to_string(),
            Some(Cell::Idle) => "empty".to_string(),
            Some(Cell::Blank) | None => format!("{}", stage.registers.len()),
        },
        FontId::proportional(10.0),
        theme.muted,
    );

    // The registers, as many as the card is tall enough for. The rest are on
    // the hover, which is why the count is always visible.
    let line = 13.0;
    let top = inner.top() + 20.0;
    let room =
        ((inner.bottom() - top - if feedback { 16.0 } else { 0.0 }) / line).max(0.0) as usize;
    // When there are more than fit, the last line goes to saying how many —
    // so one fewer name is drawn, rather than a name and the count on top of
    // each other.
    let (shown, hidden) = if stage.registers.len() > room {
        let shown = room.saturating_sub(1);
        (shown, stage.registers.len() - shown)
    } else {
        (stage.registers.len(), 0)
    };
    for (row, name) in stage.registers.iter().take(shown).enumerate() {
        painter.text(
            egui::Pos2::new(inner.left(), top + line * row as f32),
            Align2::LEFT_TOP,
            elide(name, 24),
            FontId::monospace(10.0),
            theme.ink,
        );
    }
    if hidden > 0 {
        painter.text(
            egui::Pos2::new(inner.left(), top + line * shown as f32),
            Align2::LEFT_TOP,
            format!("+ {hidden} more"),
            FontId::monospace(10.0),
            theme.muted,
        );
    }

    // Feedback is a fact about the structure, so it shows whether or not a dump
    // is open.
    if feedback {
        painter.text(
            inner.left_bottom(),
            Align2::LEFT_BOTTOM,
            "↻ feeds itself",
            FontId::proportional(10.0),
            theme.warn,
        );
    }
}

/// The one clock between two stages.
fn arrow(painter: &egui::Painter, gap: egui::Rect, colour: egui::Color32) {
    let y = gap.top() + 14.0;
    let (from, to) = (gap.left() + 4.0, gap.right() - 4.0);
    painter.line_segment(
        [egui::Pos2::new(from, y), egui::Pos2::new(to, y)],
        egui::Stroke::new(1.2, colour),
    );
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::Pos2::new(to, y),
            egui::Pos2::new(to - 5.0, y - 3.5),
            egui::Pos2::new(to - 5.0, y + 3.5),
        ],
        colour,
        egui::Stroke::NONE,
    ));
}

/// A name too long for the card it is in.
fn elide(text: &str, characters: usize) -> String {
    if text.chars().count() <= characters {
        return text.to_string();
    }
    // The tail is the part that distinguishes `u_rx.u_align.valid_d1` from its
    // neighbours; the hierarchy in front of it is shared with all of them.
    let kept: String = text.chars().skip(text.chars().count() - characters + 1).collect();
    format!("…{kept}")
}

/// How wide the line-number gutter is.
const GUTTER: f32 = 52.0;

/// The RTL itself, at the line something came from.
///
/// Every other view in this window is derived from the source and names the
/// line it came from; this is where those names lead. The file is read from
/// disk rather than from the IR — nothing keeps the text after the front end
/// has run — which also means what is shown is what is *there*, not what was
/// parsed. Those differ exactly when the file has been edited since, and that
/// is worth seeing rather than hiding.
pub fn source(
    ui: &mut Ui,
    files: &FileTable,
    span: Span,
    lines: Result<&[String], &str>,
    scroll_to_target: &mut u8,
    nets: &BTreeSet<String>,
    generated: &[rtlscope_ir::Generated],
) -> Option<ViewAction> {
    let theme = Theme::of(ui);
    let mut action = None;

    let full = files.render(span);
    let short = full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string();
    // Coloured as what it is: a design written in Veryl is shown as the
    // `.veryl` every span points into, and that file has its own words.
    let lang = crate::source::Language::of(files.path(span.file));
    // The other side of this file, when a tool stands between the two: the
    // SystemVerilog written from a `.veryl`, or the `.veryl` it was written
    // from. The design is placed in the original, so that is what every click
    // lands in; the generated file is where a reader goes to see what the
    // tool made of it — which is the answer to most surprises.
    let other = generated.iter().find_map(|pair| {
        if pair.original == span.file {
            Some((pair.generated, &pair.map, true))
        } else if pair.generated == span.file {
            Some((pair.original, &pair.map, false))
        } else {
            None
        }
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new(short).monospace().strong()).on_hover_text(&full);
        if let Some((file, map, to_generated)) = other {
            let name = files
                .path(file)
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "the other file".to_string());
            let hint = match to_generated {
                true => "The SystemVerilog the tool wrote from this file, at this line",
                false => "The file this was written from, at this line",
            };
            if ui.button(format!("show {name}")).on_hover_text(hint).clicked() {
                // The same line on the other side, or the top of the file
                // when the map says nothing about this line: a reader who
                // asked to see the file should see it either way.
                let at = match to_generated {
                    true => rtlscope_sv::generated_position(map, span.line),
                    false => rtlscope_sv::original_position(map, span.line, span.col)
                        .map(|(_, line, col)| (line, col)),
                };
                let (line, col) = at.unwrap_or((1, 1));
                action = Some(ViewAction::ShowSource(Span::new(file, line, col, 1)));
            }
        }
        // The way to change a file. RTLScope shows the design and writes patches
        // to it; typing into it is the editor's job, and having a second one
        // here would mean two programs holding the same file open.
        if ui.button("open in editor").on_hover_text("Open this line where you edit").clicked() {
            action = Some(ViewAction::OpenEditor(span));
        }
        if !nets.is_empty() {
            ui.label(
                RichText::new(
                    "click a signal name to trace it — and to put it on the waveform, when one \
                     is open",
                )
                .small()
                .color(Theme::of(ui).muted),
            );
        }
    });
    ui.separator();

    let lines = match lines {
        Ok(lines) => lines,
        Err(why) => {
            empty_state(ui, "cannot show this file", why);
            return action;
        }
    };
    // Spans are 1-based; rows are not.
    let target = (span.line as usize).saturating_sub(1);

    // Every row is painted at exactly this height, and the scroll is asked for
    // by handing egui the target row's rectangle rather than by computing an
    // offset. Arithmetic on the offset has to agree with what the scroll area
    // believes about its own content, and on the frame a file first appears it
    // believes nothing — which is how a target 83 lines down lands on line 60.
    let font = FontId::monospace(12.0);
    let text_height = ui.ctx().fonts_mut(|fonts| fonts.row_height(&font));
    let row_height = text_height + 4.0;
    let character = ui.ctx().fonts_mut(|fonts| fonts.glyph_width(&font, ' ')).max(1.0);
    let widest = lines.iter().map(|line| line.chars().count()).max().unwrap_or(0);
    let content = Vec2::new(
        (GUTTER + character * widest as f32 + 16.0).max(ui.available_width()),
        row_height * lines.len() as f32,
    );

    // The target itself, within its line. The line says where to look and the
    // mark says at what: a span is a name, or a statement, and the eye should
    // land on it rather than search the row for it. When it is one name, every
    // other place the file uses that name is marked faintly as well — which is
    // the reading a trace is: where else this signal is written and read.
    let focus = lines.get(target).and_then(|line| {
        let range = crate::source::span_range(line, span.col, span.len)?;
        let word = &line[range.clone()];
        let name = nets.contains(word).then(|| word.to_string());
        Some((range, name))
    });

    ScrollArea::both().id_salt("source").auto_shrink(false).show_viewport(ui, |ui, viewport| {
        let (rect, response) = ui.allocate_exact_size(content, egui::Sense::click());
        let origin = rect.left_top();
        let painter = ui.painter_at(rect);

        // Which name the pointer is over, if any. Worked out once and used
        // twice — to underline it, and to know what a click asked for — so the
        // thing that lights up and the thing that happens cannot disagree.
        let over = response.hover_pos().and_then(|at| {
            let row = ((at.y - origin.y) / row_height) as usize;
            let column = ((at.x - origin.x - GUTTER) / character).floor();
            if column < 0.0 {
                return None;
            }
            let line = lines.get(row)?;
            let (range, word) = crate::source::word_at(line, column as usize)?;
            nets.contains(word).then(|| (row, range, word.to_string()))
        });
        if over.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if response.clicked()
            && let Some((_, _, word)) = &over
        {
            action = Some(ViewAction::PickNamed(word.clone()));
        }

        // Only the rows on screen: a file is thousands of lines and laying out
        // every one of them each frame would make this crawl.
        let first = ((viewport.min.y / row_height) as usize).saturating_sub(1).min(lines.len());
        let last = ((viewport.max.y / row_height) as usize + 2).min(lines.len());

        // A block comment can open above the first row on screen, so the state
        // is caught up over everything before it rather than assumed shut.
        let mut in_block = false;
        for line in lines.iter().take(first) {
            let _ = crate::source::pieces(line, &mut in_block, lang);
        }

        for (row, line) in lines.iter().enumerate().take(last).skip(first) {
            let top = origin.y + row_height * row as f32;
            let line_rect = egui::Rect::from_min_size(
                egui::Pos2::new(origin.x, top),
                Vec2::new(content.x, row_height),
            );
            if row == target {
                painter.rect_filled(line_rect, 0.0, theme.accent_soft);
            }
            // Under the text, so the letters keep their own colour: every
            // other use of the target's name faintly, and the target itself
            // plainly.
            let mark = |range: &std::ops::Range<usize>, colour: Color32| {
                let before = line[..range.start].chars().count() as f32;
                let width = line[range.clone()].chars().count() as f32;
                let x = origin.x + GUTTER + character * before;
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::Pos2::new(x, top + 1.0),
                        Vec2::new(character * width, row_height - 2.0),
                    ),
                    2.0,
                    colour,
                );
            };
            if let Some((_, Some(name))) = &focus {
                for range in crate::source::occurrences(line, name) {
                    mark(&range, theme.accent.gamma_multiply(0.14));
                }
            }
            if row == target
                && let Some((range, _)) = &focus
            {
                mark(range, theme.accent.gamma_multiply(0.35));
            }
            painter.text(
                egui::Pos2::new(origin.x + GUTTER - 8.0, top + row_height / 2.0),
                Align2::RIGHT_CENTER,
                (row + 1).to_string(),
                FontId::monospace(10.0),
                if row == target { theme.accent } else { theme.muted },
            );
            let job = layout(line, &mut in_block, lang, font.clone(), theme);
            let galley = painter.layout_job(job);
            let text_top = top + (row_height - text_height) / 2.0;
            painter.galley(egui::Pos2::new(origin.x + GUTTER, text_top), galley, theme.ink);

            // The name under the pointer, underlined where it sits. Drawn over
            // the line rather than built into the layout because only one name
            // on the whole file is ever marked, and rebuilding every row's
            // colours to say so would be a lot of work to draw one rule.
            if let Some((marked, range, _)) = &over
                && *marked == row
            {
                let before = line[..range.start].chars().count() as f32;
                let width = line[range.clone()].chars().count() as f32;
                let x = origin.x + GUTTER + character * before;
                let y = text_top + text_height - 1.0;
                painter.line_segment(
                    [egui::Pos2::new(x, y), egui::Pos2::new(x + character * width, y)],
                    egui::Stroke::new(1.0, theme.accent),
                );
            }
        }

        // Asked for by rectangle, so it works whether or not the target is one
        // of the rows just painted. Repeated over a few frames because the
        // first one is laid out before the panel has settled.
        if *scroll_to_target > 0 {
            let at = egui::Rect::from_min_size(
                egui::Pos2::new(origin.x, origin.y + row_height * target as f32),
                Vec2::new(1.0, row_height),
            );
            ui.scroll_to_rect(at, Some(egui::Align::Center));
            *scroll_to_target -= 1;
            ui.ctx().request_repaint();
        }
    });
    action
}

/// The file, editable.
///
/// A separate mode rather than the only mode, and the reason is measurable: the
/// viewer above lays out only the rows on screen, while a text box lays out the
/// whole file every frame — and the designs this opens run to three thousand
/// lines. The viewer is also what every other view links *into*: it lands on a
/// line and marks it, which a text box does not. So reading stays the default
fn append_line(
    job: &mut egui::text::LayoutJob,
    line: &str,
    in_block: &mut bool,
    lang: crate::source::Language,
    font: &FontId,
    theme: &Theme,
) {
    use crate::source::Piece;
    for (range, piece) in crate::source::pieces(line, in_block, lang) {
        let colour = match piece {
            Piece::Plain => theme.ink,
            Piece::Keyword => theme.accent,
            Piece::Comment => theme.muted,
            Piece::Text => theme.clock,
            Piece::Number => theme.ok,
        };
        job.append(
            &line[range],
            0.0,
            egui::TextFormat { font_id: font.clone(), color: colour, ..Default::default() },
        );
    }
}

/// One line on its own, for the viewer, which paints a row at a time.
fn layout(
    line: &str,
    in_block: &mut bool,
    lang: crate::source::Language,
    font: FontId,
    theme: &Theme,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    append_line(&mut job, line, in_block, lang, &font, theme);
    job
}

/// Two signals, and how many clocks lie between them.
///
/// Sits under the pipeline's own header because it is the same question asked
/// of two points instead of the whole design — and because a reader who has
/// just been told the design is four deep is exactly the reader who wants to
/// know how deep *this* road is.
fn depth_strip(ui: &mut Ui, pane: &mut DepthPane, files: &FileTable) -> Option<ViewAction> {
    use rtlscope_analyse::{DepthError, DepthWarning};

    let theme = Theme::of(ui);
    let mut action = None;

    ui.horizontal(|ui| {
        ui.label(RichText::new("clocks from").small().color(theme.muted));
        let asked = ui.add(
            egui::TextEdit::singleline(&mut pane.from).desired_width(120.0).hint_text("in_data"),
        );
        ui.label(RichText::new("to").small().color(theme.muted));
        let asked_to = ui.add(
            egui::TextEdit::singleline(&mut pane.to).desired_width(120.0).hint_text("out_data"),
        );

        let ready = !pane.from.trim().is_empty() && !pane.to.trim().is_empty();
        let pressed = ui.add_enabled(ready, egui::Button::new("count")).clicked();
        // Enter in either box means the same as the button: a question typed
        // in two fields is finished by the keyboard, not by reaching for a
        // mouse.
        let entered = (asked.lost_focus() || asked_to.lost_focus())
            && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if ready && (pressed || entered) {
            action = Some(ViewAction::MeasureDepth);
        }
    });

    let Some(report) = &pane.report else { return action };

    for error in &report.errors {
        let said = match error {
            DepthError::CrossDomain { clocks, .. } => {
                format!("not on one clock: the road passes {}", clocks.join(" and "))
            }
            DepthError::CombLoop { signals } => {
                format!("a value on the road decides itself: {}", signals.join(" -> "))
            }
            DepthError::NoPath { from, to } => {
                format!("nothing that leaves `{from}` arrives at `{to}`")
            }
            DepthError::UnknownSignal { name, candidates } => match candidates.first() {
                Some(near) => format!("no signal `{name}` — did you mean `{near}`?"),
                None => format!("no signal `{name}`"),
            },
        };
        ui.label(RichText::new(said).color(theme.err));
    }
    if !report.errors.is_empty() {
        return action;
    }

    ui.horizontal_wrapped(|ui| {
        let count = match (report.min_stages, report.max_stages) {
            (Some(min), Some(max)) if min == max => format!("{min} clock(s)"),
            (Some(min), Some(max)) => format!("{min} to {max} clock(s)"),
            (Some(min), None) => format!("at least {min} clock(s)"),
            _ => "no answer".to_string(),
        };
        ui.label(RichText::new(count).strong());
        if let Some(clock) = &report.clock {
            ui.label(RichText::new(format!("on {clock}")).small().color(theme.muted));
        }
        for warning in &report.warnings {
            let (text, colour) = match warning {
                DepthWarning::Reconvergent { min, max } => {
                    (format!("W001 {min} one way, {max} the other"), theme.warn)
                }
                DepthWarning::Gated { registers, .. } => {
                    (format!("W002 {} gated", registers.len()), theme.warn)
                }
                DepthWarning::Truncated { shown, .. } => {
                    (format!("W004 {shown} of more"), theme.muted)
                }
            };
            crate::theme::badge(ui, &text, colour, theme.warn_soft);
        }
        if report.feedback {
            crate::theme::badge(ui, "feeds itself", theme.muted, theme.surface_alt);
        }
    });

    for problem in &report.problems {
        ui.label(RichText::new(problem).small().color(theme.muted));
    }

    // The registers, as things to press. The span is in the report for exactly
    // this: a number is an answer, and the logic it is about is where the
    // reader was going next.
    for path in &report.paths {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{}:", path.stages)).small().color(theme.muted));
            if path.registers.is_empty() {
                ui.label(RichText::new("one clock's worth of logic").small().color(theme.muted));
            }
            for register in &path.registers {
                let gated = path.gated_at.contains(&register.name);
                let label = match gated {
                    true => format!("{} (gated)", register.name),
                    false => register.name.clone(),
                };
                if ui
                    .small_button(RichText::new(label).monospace())
                    .on_hover_text(files.render(register.span))
                    .clicked()
                {
                    action = Some(ViewAction::ShowSource(register.span));
                }
            }
        });
    }

    if let Some(measured) = &pane.measured {
        ui.separator();
        let Some(min) = measured.min else {
            ui.label(
                RichText::new("no beats were paired, so there is nothing to measure")
                    .small()
                    .color(theme.muted),
            );
            return action;
        };
        let (median, max) = (measured.median.unwrap_or(min), measured.max.unwrap_or(min));
        ui.label(
            RichText::new(format!(
                "measured: {min} shortest, {median} typical, {max} longest over {} beat(s)",
                measured.samples
            ))
            .small(),
        );

        // The bars are what make a spread visible. Numbers alone read as one
        // answer with error bars, which is the reading a histogram exists to
        // prevent.
        let widest = measured.histogram.iter().map(|bin| bin.count).max().unwrap_or(1).max(1);
        for bin in &measured.histogram {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{:>4}", bin.cycles)).monospace().color(theme.muted),
                );
                let (rect, _) = ui.allocate_exact_size(Vec2::new(160.0, 9.0), egui::Sense::hover());
                let width = 160.0 * bin.count as f32 / widest as f32;
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(rect.min, Vec2::new(width, rect.height())),
                    2.0,
                    theme.accent,
                );
                ui.label(RichText::new(bin.count.to_string()).small().color(theme.muted));
            });
        }
        for problem in &measured.problems {
            ui.label(RichText::new(problem).small().color(theme.muted));
        }
        for line in &pane.cross {
            ui.label(RichText::new(line).small().color(theme.ink));
        }
    }

    action
}

/// The window before any design is open.
///
/// This is the first thing someone sees who double-clicked the binary, so it
/// has to be an invitation rather than an error: what the tool is, what it
/// takes, and — when a drop has just failed — why. Returns a span if the
/// reader clicked one of the reasons.
pub fn welcome(
    ui: &mut Ui,
    hovering: bool,
    failed: Option<(&FileTable, &Diagnostics, &str)>,
    tops: &[String],
) -> Welcome {
    let theme = Theme::of(ui);
    let mut action = Welcome::default();

    // The whole panel is the drop target, so the outline goes round all of it
    // rather than round some smaller rectangle the file has to be aimed at.
    let area = ui.available_rect_before_wrap();
    if hovering {
        ui.painter().rect_filled(area, egui::CornerRadius::same(8), theme.accent_soft);
        ui.painter().rect_stroke(
            area.shrink(6.0),
            egui::CornerRadius::same(8),
            egui::Stroke::new(2.0, theme.accent),
            egui::StrokeKind::Inside,
        );
    }

    // A quarter of the height, the same headroom the file's other empty
    // states take. Much less when a drop has just failed, because then the
    // reasons underneath are what the page is for.
    ui.add_space(if failed.is_some() { 24.0 } else { area.height() * 0.25 });
    ui.vertical_centered(|ui| {
        ui.horizontal(|ui| {
            // Centred by allocating the pair together: two centred children
            // would each centre themselves and drift apart.
            ui.add_space((ui.available_width() - 120.0).max(0.0) / 2.0);
            crate::theme::brand_mark(ui, theme);
            ui.label(RichText::new("RTLScope").heading().strong());
        });
        ui.add_space(2.0);
        ui.label(
            RichText::new("Read synthesisable SystemVerilog, and look at what it says.").weak(),
        );
        ui.add_space(18.0);

        ui.label(
            RichText::new(if hovering { "let go" } else { "Drop a folder or files here" })
                .heading()
                .color(theme.accent),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "a project folder · sources (.sv .v) · a file list (.f) · \
                 a waveform (.vcd .fst) · results.xml",
            )
            .small()
            .weak(),
        );
        ui.add_space(10.0);
        // Or choose them. The same things, through the system's own dialog,
        // for a reader whose files are not already in a window beside this
        // one. Centred as one row, the way the brand is above.
        ui.horizontal(|ui| {
            ui.add_space((ui.available_width() - 300.0).max(0.0) / 2.0);
            ui.label(RichText::new("or open").weak());
            if ui
                .button("files…")
                .on_hover_text("Sources, a file list, a waveform or results  (Ctrl+O)")
                .clicked()
            {
                action.browse = Some(Browse::Files);
            }
            if ui.button("a folder…").on_hover_text("Every .sv and .v under it").clicked() {
                action.browse = Some(Browse::Folder);
            }
            if ui.button("a waveform…").on_hover_text("A .vcd or .fst, on its own").clicked() {
                action.browse = Some(Browse::Waveform);
            }
        });
        ui.add_space(14.0);
        ui.label(
            RichText::new("rtlscope-gui <files or folder> --top <module>")
                .monospace()
                .small()
                .weak(),
        )
        .on_hover_text("The same thing from a shell, which also takes -f, -D and -I.");
    });

    // A drop that did not come out as a design: the reasons are the answer.
    if let Some((files, diags, what)) = failed {
        ui.add_space(18.0);
        ui.separator();

        // Before the diagnostics, because it is the answer to them. The
        // command line says `--top`; a window that only repeated that would be
        // telling the reader to close it and start again.
        if !tops.is_empty() {
            ui.add_space(6.0);
            ui.label(RichText::new("more than one design here — which one?").strong());
            ui.label(RichText::new("every module below is instantiated by nothing").small().weak());
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for name in tops {
                    if ui.button(RichText::new(name).monospace()).clicked() {
                        action.top = Some(name.clone());
                    }
                }
            });
            ui.add_space(10.0);
        }
        ui.horizontal(|ui| {
            badge(ui, "could not elaborate", theme.err, theme.err_soft);
            ui.label(RichText::new(what).monospace());
            ui.label(RichText::new("— nothing was hidden; here is why").small().weak());
        });
        let rows: Vec<(Severity, &'static str, String, Option<Span>)> = diags
            .iter()
            .map(|diag| (diag.severity, diag.code.id(), diag.message.clone(), diag.span))
            .collect();
        ScrollArea::vertical().id_salt("welcome-diagnostics").auto_shrink(false).show(ui, |ui| {
            for (severity, code, message, span) in &rows {
                let (strong, soft) = match severity {
                    Severity::Error => (theme.err, theme.err_soft),
                    Severity::Warning => (theme.warn, theme.warn_soft),
                    Severity::Info => (theme.muted, theme.surface_alt),
                };
                ui.horizontal_wrapped(|ui| {
                    badge(ui, code, strong, soft);
                    ui.label(message);
                    if let Some(span) = span
                        && location(ui, files, *span)
                    {
                        action.open = Some(*span);
                    }
                });
            }
        });
    }
    action
}

/// What a file dialog is asked for.
///
/// Several dialogs rather than one, because the filter is the help: a dialog
/// that shows every file on the disk has told the reader nothing about what
/// this program reads.
///
/// The two `Add` ones open the same dialogs as `Files` and `Folder`. What
/// differs is what is done with the answer: those replace the open design,
/// these join it. It is carried here rather than as a flag beside it so that a
/// dialog cannot come back and have to guess which question it was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browse {
    /// Sources, a file list, a waveform or results — anything a drop takes.
    Files,
    /// A project folder, gathered the way a dropped one is.
    Folder,
    /// A recording, on its own or beside the design.
    Waveform,
    /// A run's `results.xml`.
    Results,
    /// Sources to read together with the ones already open.
    AddFiles,
    /// A folder to read together with the ones already open.
    AddFolder,
    /// A testbench the reader wrote, to run against the design.
    Testbench,
}

/// What the welcome screen was asked for.
#[derive(Default)]
pub struct Welcome {
    /// A source location to open in the editor.
    pub open: Option<Span>,
    /// A module chosen as the top, to read the same files again with.
    pub top: Option<String>,
    /// A file dialog to open.
    pub browse: Option<Browse>,
}

/// Where the open testbench's name is left for the simulate button.
///
/// Through `egui`'s own store rather than another argument threaded down two
/// call sites: the button is drawn from two views that know nothing about a
/// testbench, and both would have to carry it past everything else they do.
fn bench_id() -> egui::Id {
    egui::Id::new("rtlscope-bench-top")
}

/// Tells the views which testbench is open, if any.
pub fn set_bench(ctx: &egui::Context, top: Option<String>) {
    ctx.data_mut(|data| match top {
        Some(top) => {
            data.insert_temp(bench_id(), top);
        }
        None => data.remove::<String>(bench_id()),
    });
}

/// The button that starts a simulation, and whatever came of the last one.
///
/// One function because the three states are one control: it is a button, then
/// a spinner, then — if nothing came of it — the reason, in the place the
/// button was. The status line along the bottom is the wrong place for that:
/// it is one row of small grey text on a window two thousand pixels wide, and
/// somebody who just pressed a button is looking at the button.
fn simulate_button(
    ui: &mut Ui,
    simulating: Option<&str>,
    refused: Option<&str>,
    cycles: &mut u64,
    range: std::ops::RangeInclusive<u64>,
) -> bool {
    let mut asked = false;
    // What the button will actually do. With a testbench open it is not
    // writing anything, and a button that said it was would be describing the
    // other half of this feature.
    let bench: Option<String> = ui.data(|data| data.get_temp(bench_id()));

    ui.vertical_centered(|ui| match simulating {
        Some(progress) => {
            ui.spinner();
            ui.label(RichText::new(progress).weak());
        }
        None if bench.is_some() => {
            let name = bench.clone().unwrap_or_default();
            asked = ui
                .button(format!("run `{name}`"))
                .on_hover_text(
                    "Your testbench, run against this design and left untouched. Its \
                     stimulus, its checks, its waveform — nothing here is generated.",
                )
                .clicked();
        }
        None => {
            asked = ui
                .button("simulate this design")
                .on_hover_text(
                    "Write a harness — clocks and reset read out of the design, every other \
                     input driven at random — run a simulator on it, and open the waveform.",
                )
                .clicked();
            ui.add_space(4.0);
            // How long the run is, beside the button that starts it. The right
            // number is a fact about the design and not about the tool: a
            // handshake settles in forty cycles, a video frame needs thousands
            // to reach anything worth looking at.
            ui.horizontal(|ui| {
                let width = ui.available_width();
                ui.add_space((width - 150.0).max(0.0) / 2.0);
                ui.label(RichText::new("for").small().weak());
                ui.add(egui::DragValue::new(cycles).range(range).speed(10.0).suffix(" cycles"))
                    .on_hover_text(
                        "How long the run is, in clock cycles. Too few and the design has not \
                     finished resetting; too many and the dump is something to store rather \
                     than something to look at.",
                    );
            });
            ui.label(
                RichText::new("random stimulus: a waveform to look at, not a verdict")
                    .small()
                    .weak(),
            );
        }
    });

    if let Some(why) = refused.filter(|_| simulating.is_none()) {
        ui.add_space(10.0);
        refusal(ui, why);
    }
    asked
}

/// Why the last run produced nothing, said where the button was pressed.
///
/// Every view that can start a simulation has to be able to draw this. A run
/// asked for from one view and refused where only another view would say so is
/// a button that does nothing — measured, from the Stim tab, whose `run` failed
/// on a broken toolchain and reported it into the waveform pane the reader was
/// not looking at.
pub fn refusal(ui: &mut Ui, why: &str) {
    let theme = Theme::of(ui);
    // Boxed and centred at a readable width. The reason a simulation refuses is
    // usually a sentence, and a sentence set across the whole window is one
    // nobody finishes.
    let width = ui.available_width().min(560.0);
    ui.vertical_centered(|ui| {
        egui::Frame::new()
            .fill(theme.err_soft)
            .stroke(egui::Stroke::new(1.0, theme.err))
            .corner_radius(6)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.set_max_width(width);
                ui.label(RichText::new("nothing was simulated").strong().color(theme.err));
                ui.add_space(3.0);
                ui.label(why);
            });
    });
}

/// The wave tab before any dump is open: how to get one, or make one.
pub fn wave_empty_state(
    ui: &mut Ui,
    simulating: Option<&str>,
    refused: Option<&str>,
    cycles: &mut u64,
    range: std::ops::RangeInclusive<u64>,
) -> Option<ViewAction> {
    let mut action = None;
    empty_state(
        ui,
        "no waveform open",
        "Drag a .vcd or .fst onto the window, or start with --dump <FILE>.\n\
         Drop a run's results.xml here too, and every failing test becomes a button.",
    );
    ui.add_space(8.0);
    if simulate_button(ui, simulating, refused, cycles, range) {
        action = Some(ViewAction::Simulate);
    }
    action
}

/// A tab with nothing to show says why, and what would make it show something.
pub(crate) fn empty_state(ui: &mut Ui, title: &str, body: &str) {
    ui.add_space(ui.available_height() * 0.25);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).heading().weak());
        ui.add_space(4.0);
        ui.label(RichText::new(body).weak());
    });
}
