//! The waveform panel.
//!
//! Not an `egui::Scene`, which is what the block diagram uses: a scene zooms in
//! two dimensions, and a waveform must zoom in one. Time and x are related by a
//! single affine map, the track heights are fixed, and vertical space is a
//! plain scroll area. That is the whole of the geometry.
//!
//! The part that needs care is drawing. A signal in a real dump has more
//! changes than the panel has pixels, and walking them every frame would make
//! an immediate-mode GUI unusable. So each track is reduced to one [`Bin`] per
//! column of pixels — what it did in the slice of time that column covers — and
//! that reduction is cached per zoom level. Panning reuses it; only zooming
//! rebuilds it.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use crate::theme::Theme;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use rtlscope_analyse::Fsm;
use rtlscope_analyse::flat::{Flattened, SignalId};
use rtlscope_analyse::pipeline::DomainDepth;
use rtlscope_ir::NetId;
use rtlscope_wave::compare::Comparison;
use rtlscope_wave::decode::{Annotation, DecodeReport, Level};
use rtlscope_wave::flow::TokenFlow;
use rtlscope_wave::stages::{Cell as StageCell, Cycles, StageView};
use rtlscope_wave::{Dump, MatchReport, WaveError, WaveValue, WaveVar};

/// How tall one signal's row is.
const TRACK_HEIGHT: f32 = 30.0;

/// The gap above and below the line, inside the track.
///
/// What is left is the swing, and the swing is what a reader measures a level
/// by. At the old 22 with 4 above and 6 below there were twelve pixels of it,
/// which is not enough to tell a square wave from a dashed line at a glance.
const TRACK_PAD: f32 = 6.0;

/// Wide enough to read as a line rather than a hair, at both grounds.
const WAVE_STROKE: f32 = 1.6;

/// The slant at each end of a bus, which is what says the value changed there.
const SHOULDER: f32 = 4.0;

/// The value written inside a bus.
const BUS_TEXT: f32 = 10.0;

/// How wide the names down the left are.
const NAME_WIDTH: f32 = 220.0;

/// How tall the time axis above the tracks is.
const RULER_HEIGHT: f32 = 22.0;

/// The fewest pixels between two labelled ticks.
///
/// Times are long numbers, so this is set by how much room one needs rather
/// than by how many marks look nice.
const TICK_SPACING: f32 = 96.0;

/// How tall the bar along the bottom is.
const SCROLL_HEIGHT: f32 = 14.0;

/// How narrow the thumb may get and still be something a hand can catch.
///
/// Zoomed into a hundredth of a recording the thumb would otherwise be a pixel
/// wide, which is a control that exists without being usable.
const THUMB_MIN: f32 = 24.0;

/// How far from a marker's flag a click still counts as hitting it.
const FLAG_SLOP: f32 = 5.0;

/// How many cycles are read the moment the stages are opened, before anything
/// has been drawn and there is a visible range to go by.
const OPENING_WINDOW: usize = 512;

/// One cycle of the pipeline, as the diagram in the other tab reads it: the
/// clock it belongs to, which cycle it is, and what each stage held.
pub type StagesAt = (String, usize, Vec<(usize, StageCell)>);

/// A value's stretch across the plot, measured in pixels.
///
/// The bins above answer "what was happening around here", which is the right
/// question for deciding whether a stretch is too dense to draw. They are the
/// wrong thing to draw *from*: a bin is a whole pixel column, so every edge
/// lands on a column boundary rather than where the change was, and a clock
/// whose half period is not a whole number of pixels comes out as a row of
/// ticks instead of a square wave. Runs keep the fractional position, and the
/// line is drawn between them.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Where it starts and ends, in pixels from the left of the plot.
    pub x0: f32,
    pub x1: f32,
    /// What it held, or `None` where too much happened to say.
    pub value: Option<WaveValue>,
    /// More changes fell in here than there are pixels to draw them in, so it
    /// is drawn as a band. Saying "something happened here, too fast to see"
    /// is honest; drawing one of the values as though it held is not.
    pub busy: bool,
}

/// A stretch narrower than this cannot show an edge, only suggest one.
const MIN_RUN: f32 = 1.5;

/// Cuts a signal's changes into the stretches it holds still for.
///
/// `t0` is the time at the left edge and `per_px` how much time a pixel
/// covers. Pure, so it can be tested without a dump or a window.
pub fn runs(changes: &[(u64, WaveValue)], t0: f64, per_px: f64, width: f32) -> Vec<Run> {
    if changes.is_empty() || per_px <= 0.0 || width <= 0.0 {
        return Vec::new();
    }
    let at = |time: u64| ((time as f64 - t0) / per_px) as f32;

    // Whatever was in force at the left edge. Nothing before the first change:
    // a plot that starts before the dump does holds nothing there, rather than
    // borrowing a value from the future.
    let mut next = 0usize;
    let mut current: Option<WaveValue> = None;
    while next < changes.len() && (changes[next].0 as f64) <= t0 {
        current = Some(changes[next].1.clone());
        next += 1;
    }

    let mut cut: Vec<Run> = Vec::new();
    let mut x0 = 0.0f32;
    while next < changes.len() {
        let (time, value) = &changes[next];
        let x1 = at(*time);
        if x1 > width {
            break;
        }
        if x1 > x0 {
            cut.push(Run { x0, x1, value: current.clone(), busy: false });
        }
        current = Some(value.clone());
        x0 = x1.max(0.0);
        next += 1;
    }
    if x0 < width {
        cut.push(Run { x0, x1: width, value: current, busy: false });
    }

    // Then the stretches too narrow to draw are folded together. One thin run
    // beside wide ones is still an edge worth drawing; a row of them is a
    // signal changing faster than the screen can say, and the band is the only
    // truthful picture of that.
    let mut folded: Vec<Run> = Vec::new();
    let mut index = 0usize;
    while index < cut.len() {
        if cut[index].x1 - cut[index].x0 >= MIN_RUN {
            folded.push(cut[index].clone());
            index += 1;
            continue;
        }
        let start = index;
        while index < cut.len() && cut[index].x1 - cut[index].x0 < MIN_RUN {
            index += 1;
        }
        // A single narrow run is an edge, not a blur.
        if index - start == 1 {
            folded.push(cut[start].clone());
            continue;
        }
        folded.push(Run { x0: cut[start].x0, x1: cut[index - 1].x1, value: None, busy: true });
    }
    folded
}

/// What asking for a track did.
///
/// Three answers, not two. "It is already there" used to come back as success
/// and be reported as `added`, which was fine while the only way in was a
/// picker that showed what was already shown — and became a lie the moment a
/// name could be clicked in the source, where a reader asking for `clk` would
/// be told it was added and see nothing change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    /// It is on the panel now and was not before.
    New,
    /// It was already there. Selected, so the answer to "where did it go" is
    /// on screen rather than in a sentence.
    Already,
    /// The dump has no such variable.
    Missing,
}

impl Added {
    /// Whether the track is on the panel, however it got there.
    pub fn shown(self) -> bool {
        !matches!(self, Added::Missing)
    }
}

/// What the design calls each matched signal's values, where it says.
///
/// Two layers, and no more. A module's `localparam`s are constants too, and
/// naming every bus value that happens to equal one would rename half the
/// panel after something it has nothing to do with. What both layers here have
/// in common is that the author *said* what the values mean: a signal declared
/// `state_e`, or a register whose `case` arms decide its own next value.
fn value_names(
    design: &rtlscope_ir::Design,
    flat: &Flattened,
    fsms: &[Fsm],
    matches: &MatchReport,
) -> HashMap<SignalId, HashMap<i64, String>> {
    let mut named = HashMap::new();
    for matched in &matches.matched {
        if let Some(names) = names_of(design, flat, fsms, matched.signal) {
            named.insert(matched.signal, names);
        }
    }
    named
}

/// What one signal's values are called: by its enum type, or by its `case`.
///
/// The enum first, because a type is a stronger claim than an inference — a
/// design that says both should be read as saying the one it wrote down.
///
/// Any home will do: a signal crossing a boundary is declared on both sides,
/// and if either side gave it an enum type then that is what its values mean.
/// The first home that names one wins.
fn names_of(
    design: &rtlscope_ir::Design,
    flat: &Flattened,
    fsms: &[Fsm],
    signal: SignalId,
) -> Option<HashMap<i64, String>> {
    enum_names_of(design, flat, signal).or_else(|| state_names(design, flat, fsms, signal))
}

/// What a `case` calls the values of the register it decides.
///
/// The layer under the enum, and only for a register [`rtlscope_analyse::fsm`]
/// has proved is a state register: one whose next value a `case` on itself
/// decides. Those arms are where the author said what its values mean, which
/// is the same claim an enum makes — so the paragraph above that refuses every
/// other `localparam` is not being contradicted here. A bus that merely
/// happens to equal one is still not named.
///
/// The next-state net gets the same names. It carries the same encoding one
/// clock early, and a panel that spelled one and numbered the other would be
/// leaving the reader to do the translation.
fn state_names(
    design: &rtlscope_ir::Design,
    flat: &Flattened,
    fsms: &[Fsm],
    signal: SignalId,
) -> Option<HashMap<i64, String>> {
    for &(node, net) in flat.homes(signal) {
        let module_id = flat.nodes[node].module;
        let module = &design.modules[module_id];
        for fsm in fsms.iter().filter(|fsm| fsm.module == module_id) {
            let next = fsm
                .next_name
                .as_deref()
                .and_then(|name| module.net_by_name(name))
                .map(|(id, _)| id);
            if fsm.state != net && next != Some(net) {
                continue;
            }
            let names = named_values(fsm);
            if !names.is_empty() {
                return Some(names);
            }
        }
    }
    None
}

/// The states whose value is theirs alone.
///
/// A value two states share is left out, which is
/// [`rtlscope_ir::EnumType::name_of`]'s rule: two names for one number means
/// the name is a matter of taste, and picking one would be inventing a fact.
/// Kept the same here so an encoding written twice reads as a number in both
/// layers rather than as a number in one and a guess in the other.
fn named_values(fsm: &Fsm) -> HashMap<i64, String> {
    let mut names: HashMap<i64, String> = HashMap::new();
    let mut shared: HashSet<i64> = HashSet::new();
    for state in &fsm.states {
        if names.insert(state.value, state.name.clone()).is_some() {
            shared.insert(state.value);
        }
    }
    for value in shared {
        names.remove(&value);
    }
    names
}

/// What one signal's enum type calls its values.
fn enum_names_of(
    design: &rtlscope_ir::Design,
    flat: &Flattened,
    signal: SignalId,
) -> Option<HashMap<i64, String>> {
    for &(node, net) in flat.homes(signal) {
        let module = &design.modules[flat.nodes[node].module];
        let Some(type_name) = module.nets[net].type_name.as_deref() else { continue };
        let Some(enumeration) = module.enums.iter().find(|it| it.name == type_name) else {
            continue;
        };
        let names: HashMap<i64, String> = enumeration
            .members
            .iter()
            .filter_map(|member| {
                Some((member.value, enumeration.member_of(member.value)?.shown().to_string()))
            })
            .collect();
        if !names.is_empty() {
            return Some(names);
        }
    }
    None
}

/// A stretch of time being dragged out.
#[derive(Debug, Clone, Copy)]
struct Band {
    /// Where the drag began, in the dump's own ticks.
    from: f64,
    /// And where it is now, likewise. Either may be the larger.
    to: f64,
}

/// Rows on their way somewhere else, while the hand is still down.
///
/// The rows are held by position, not by name, because a move is the one
/// gesture during which nothing else may touch the list: no row is added,
/// removed or renamed between the press and the release, so the positions
/// taken at the press are still the right ones at the drop.
#[derive(Debug, Clone)]
struct Moving {
    /// What was taken hold of, lowest first.
    rows: Vec<usize>,
    /// The line boundary the drop would land at: `0` is above the first line,
    /// `lines` below the last. A boundary and not a line, because what a hand
    /// aims at when it reorders a list is the gap between two rows.
    at: usize,
}

/// What a finished drag turned out to mean.
///
/// A pure decision so it can be tested without a pointer: the difference
/// between a zoom and a click is a threshold, and a threshold nobody can see
/// the far side of is one that gets tuned by guesswork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dragged {
    /// Wide enough to mean a stretch of time.
    Zoom,
    /// Narrow enough that the reader meant to press, not to sweep.
    Click,
}

/// How far a drag has to travel before it is a zoom rather than a press.
///
/// Four pixels: below that a click on a trackpad routinely registers as a drag
/// of one or two, and a viewer that zoomed to a two-pixel window every time
/// somebody set the cursor would be unusable.
const BAND_SLOP: f32 = 4.0;

/// Whether a drag from one pixel to another was a zoom or a click.
pub fn dragged(from_x: f32, to_x: f32) -> Dragged {
    match (to_x - from_x).abs() >= BAND_SLOP {
        true => Dragged::Zoom,
        false => Dragged::Click,
    }
}

/// The scrollbar's arithmetic.
///
/// Every number here is a ratio between two spans — how much of the recording
/// is on screen, and how far the thumb may therefore slide — and all of them
/// are wanted in three places: drawing the thumb, dragging it, and pressing the
/// groove beside it. Worked out once and passed around, so the three cannot
/// come to different conclusions about where the thumb is.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bar {
    /// The whole recording, in ticks. Never zero.
    span: f64,
    /// How much of it the plot shows, in ticks. Never more than `span`.
    window: f64,
    /// The thumb's width in pixels, never below [`THUMB_MIN`].
    thumb: f32,
    /// How far the thumb may slide, in pixels. Zero when it all fits.
    travel: f32,
}

impl Bar {
    fn of(per_px: f64, plot_width: f32, end: u64, width: f32) -> Bar {
        let span = end.max(1) as f64;
        let window = (per_px * f64::from(plot_width.max(1.0))).clamp(0.0, span);
        // The thumb says what fraction is on screen, except that below a
        // certain size it stops saying anything and starts being unusable.
        let thumb = ((window / span) as f32 * width).clamp(THUMB_MIN.min(width), width);
        Bar { span, window, thumb, travel: (width - thumb).max(0.0) }
    }

    /// Whether there is anything off screen to scroll to.
    fn scrolls(&self) -> bool {
        self.travel > 0.0 && self.span > self.window
    }

    /// How much time is off screen — what the whole travel is worth.
    fn reach(&self) -> f64 {
        (self.span - self.window).max(0.0)
    }

    /// Where the thumb's left edge sits, given where the view starts.
    fn offset(&self, t_left: f64) -> f32 {
        match self.scrolls() {
            true => ((t_left / self.reach()) as f32 * self.travel).clamp(0.0, self.travel),
            false => 0.0,
        }
    }

    /// Where the view starts, given where the thumb's left edge is put.
    fn t_left(&self, offset: f32) -> f64 {
        match self.scrolls() {
            true => (f64::from(offset.clamp(0.0, self.travel)) / f64::from(self.travel)
                * self.reach())
            .clamp(0.0, self.reach()),
            false => 0.0,
        }
    }

    /// Where the view starts when the groove is pressed `x` pixels along.
    ///
    /// That moment ends up in the middle of the plot rather than at its left
    /// edge: a reader pressing the bar is pointing at something they want to
    /// see, and putting it against the frame shows half of whatever it is.
    fn centred(&self, x: f32, width: f32) -> f64 {
        let at = f64::from(x.clamp(0.0, width)) / f64::from(width.max(1.0)) * self.span;
        (at - self.window / 2.0).clamp(0.0, self.reach())
    }
}

/// Where a stretch of time puts the view: its left edge, and what a pixel of
/// it is worth.
///
/// The ends arrive as raw times, and either may be the larger — a drag runs
/// whichever way the reader's hand went, and off the front of the recording if
/// they overshoot. Time before zero is clamped here rather than at the call
/// site, so there is one place that decides and one place to read.
///
/// `None` for a stretch with no width. Zooming to a single instant has no
/// scale — every pixel would be the same tick — and the gesture that produces
/// one is a click, which means something else.
fn zoomed(from: f64, to: f64, width: f32) -> Option<(f64, f64)> {
    let near = from.min(to).max(0.0);
    let far = from.max(to).max(0.0);
    if far <= near {
        return None;
    }
    Some((near, ((far - near) / f64::from(width.max(1.0))).max(f64::MIN_POSITIVE)))
}

/// One row of the panel.
pub enum Track {
    /// A signal from the dump.
    ///
    /// `signal` is the wire of the design it was matched to, and it is what
    /// makes a track more than a picture: without it a reader looking at a
    /// waveform has no way back to the module, the source line, or the
    /// question of where the value came from. `None` when the dump has it and
    /// the design does not — a testbench's own signals, mostly.
    Signal { var: WaveVar, name: String, width: u32, signal: Option<SignalId> },
    /// One lane of a decoder's annotations.
    Decoded { decode: usize, row: u8, name: String },
    /// One depth of the pipeline, a cell per clock cycle.
    Stage { stage: usize, name: String },
}

/// Which recording a set of bins belongs to.
///
/// Part of the cache key rather than a second cache, so that every place that
/// throws the bins away on a zoom throws both away. Two caches would be two
/// chances to forget one, and a stale ghost is a picture that lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Layer {
    Recorded,
    Reference,
}

/// A second recording, for this one to be held against.
///
/// The comparison is made once, when it is opened, rather than per frame: it
/// reads signals, and reading is the expensive half.
pub struct Reference {
    pub path: std::path::PathBuf,
    dump: Dump,
    pub comparison: Comparison,
    /// The paths that differ, for badging a row without searching the list.
    differing: HashSet<String>,
    /// The reference's own changes, per track position, already in *this*
    /// dump's ticks so that everything drawn shares one timebase.
    changes: HashMap<usize, Vec<(u64, WaveValue)>>,
    /// Whether the two count time differently, decided once when it opened.
    convert: bool,
}

impl Reference {
    /// Whether the two recordings part on this signal.
    pub fn differs(&self, path: &str) -> bool {
        self.differing.contains(path)
    }
}

/// A dump, opened, matched, and being looked at.
pub struct WaveState {
    pub dump: Dump,
    pub path: std::path::PathBuf,
    pub matches: MatchReport,
    pub tracks: Vec<Track>,
    pub decodes: Vec<DecodeReport>,

    /// The time at the left edge, and how much time a pixel covers.
    t_left: f64,
    per_px: f64,
    /// What each signal's enum type calls the values it holds.
    ///
    /// Copied in when the design is read rather than looked up as it draws:
    /// this holds a dump, not a design, and a panel that reached back into the
    /// design for every label would have to be handed one at every call site
    /// that paints a row. The same reason the matching is copied in.
    named: HashMap<SignalId, HashMap<i64, String>>,
    /// Set when the view should be sized to the whole recording, and cleared
    /// once it has been.
    ///
    /// Fitting needs the width of the plot, and the two places that ask for it
    /// — opening a dump, and the `fit` button — are both outside the layout
    /// that decides it. They used to guess: 800 pixels at open and the
    /// toolbar's leftover width at the button. Measured against a 1150-pixel
    /// plot the first showed half again as much time as the recording has,
    /// which is a third of the panel spent on nothing and a scrollbar stuck at
    /// full width for the first few notches of zooming in. So the ask is
    /// recorded and answered where the width is known, which is what the ruler
    /// already does.
    pub refit: bool,
    /// Where the pointer was when the menu was asked for.
    ///
    /// Caught at the click rather than read when the item is pressed: a menu is
    /// open for as long as it takes to read it, and the hand moves in that
    /// time. Marking wherever the pointer drifted to would put the flag
    /// somewhere nobody pointed at.
    menu_at: Option<u64>,
    /// The stretch being dragged out, while it is being dragged.
    ///
    /// The anchor is kept as a *time* rather than as the pixel it started at:
    /// the view can move under a drag — the wheel still zooms — and a band
    /// pinned to a pixel would then cover a different stretch than the one the
    /// reader put their finger on.
    band: Option<Band>,
    /// The rows being dragged to a new place, while they are being dragged.
    ///
    /// Kept on the state rather than in egui's memory for the same reason the
    /// band is: it has to survive the frames between the press and the release,
    /// and it is what the insertion line is drawn from.
    moving: Option<Moving>,
    pub cursor: Option<u64>,

    /// Changes per signal, read once when a track is added.
    changes: HashMap<usize, Vec<(u64, WaveValue)>>,
    /// Bins per (layer, track, zoom level). Panning reuses these; zooming
    /// rebuilds.
    /// The recording this one is being held against, when there is one.
    pub reference: Option<Reference>,
    pub status: String,

    /// The rows the reader is pointing at, in the order they were picked.
    ///
    /// **The last one is the anchor**, and one row is all that most of this
    /// means: an edge belongs to one signal, so does a declaration, so does a
    /// wire in the diagram. The rest are here for the one kind of work that is
    /// about several rows at once — taking them off the panel, which at one
    /// click a row is the thing that makes a panel full of a bus tedious to
    /// clear.
    ///
    /// A list rather than a set, because which one is the anchor is a fact
    /// about the order they were picked in, and a set would lose it.
    selection: Vec<usize>,
    /// The anchor as it was last reported to the workbench.
    ///
    /// So a move is noticed once. Kept beside the selection rather than in the
    /// workbench because the panel is what moves it, and a copy on the other
    /// side of the call would go stale whenever a track was removed.
    announced: Option<usize>,
    /// Named moments, kept sorted. The distance from the cursor to the nearest
    /// is what the toolbar measures, which is how a protocol's latency gets
    /// counted without arithmetic on two numbers read off a screen.
    markers: Vec<u64>,
    /// What to look for, as the reader typed it.
    search: String,
    /// Whether the column carries whole paths or the hierarchy.
    pub names: Names,
    /// The scopes folded shut, by their whole path.
    ///
    /// What is folded rather than what is open, so a scope that appears when a
    /// signal is added arrives open. The other way round, every new module
    /// would come in shut and the reader would have to open what they just
    /// asked for.
    shut: HashSet<String>,

    /// The signal picker is open.
    picking: bool,
    /// The picker's tree, and the filter it was built for.
    picker: Option<(String, Scope)>,
    filter: String,
    /// The buses last proposed for the module being looked at.
    pub buses: Vec<rtlscope_wave::Suggestion>,

    /// The clock domain being laid out in cycles, and its rising edges.
    domain: Option<DomainDepth>,
    cycles: Option<Cycles>,
    /// The occupancy of the window last built, and which window that was.
    stage_view: Option<StageView>,
    stage_window: (usize, usize),
    /// The tokens followed through that same window, worked out once. Keyed by
    /// the window so it cannot be shown against occupancy it did not come from.
    token_flow: Option<((usize, usize), TokenFlow)>,

    /// What the run that produced this dump came to, when it left a results
    /// file next to it.
    pub results: Option<rtlscope_tb::Run>,
}

impl WaveState {
    /// Runs a decoder and gives each lane of its annotations a track.
    ///
    /// The same call the CLI makes, so what is drawn and what is printed cannot
    /// differ.
    pub fn decode(&mut self, protocol: &str, bindings: &[rtlscope_wave::Binding]) {
        let Some(decoder) = rtlscope_wave::decode::by_name(protocol) else {
            self.status = format!("no decoder called `{protocol}`");
            return;
        };
        let resolved = match rtlscope_wave::ResolvedBindings::resolve(
            &mut self.dump,
            decoder.channels(),
            bindings,
        ) {
            Ok(resolved) => resolved,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        let report = decoder.decode(&self.dump, &resolved);

        let mut rows: Vec<u8> = report.annotations.iter().map(|a| a.row).collect();
        rows.sort_unstable();
        rows.dedup();
        let index = self.decodes.len();

        let summary: Vec<String> =
            report.stats.iter().take(3).map(|(name, value)| format!("{value} {name}")).collect();
        self.status = if report.problems.is_empty() {
            format!("{protocol}: {}", summary.join(", "))
        } else {
            format!(
                "{protocol}: {} — {} thing(s) could not be accounted for",
                summary.join(", "),
                report.problems.len()
            )
        };

        for row in rows {
            self.tracks.push(Track::Decoded {
                decode: index,
                row,
                name: format!("{protocol} {}", lane_name(&report, row)),
            });
        }
        self.decodes.push(report);
    }

    /// Every bus the design suggests for a module, ready to run.
    pub fn buses(
        &self,
        design: &rtlscope_ir::Design,
        module: rtlscope_ir::ModuleId,
        instance: &str,
    ) -> Vec<rtlscope_wave::Suggestion> {
        let prefix = self.matches.prefix.clone();
        rtlscope_wave::bind::suggest(design, module, instance)
            .into_iter()
            .map(|mut suggestion| {
                suggestion.bindings = suggestion.mapped(&prefix);
                suggestion
            })
            .collect()
    }

    /// The row the keyboard and the ways back into the design act on.
    pub fn selected(&self) -> Option<usize> {
        self.selection.last().copied()
    }

    /// Every row the reader has picked, lowest first.
    pub fn selected_rows(&self) -> Vec<usize> {
        let mut rows = self.selection.clone();
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    /// Points at one row and nothing else, and opens whatever was folded over
    /// it — a row nobody can see is not something this can be said to point at.
    pub fn select_only(&mut self, row: usize) {
        self.selection = vec![row];
        self.reveal(row);
    }

    /// A name clicked, with whatever was held down while it was.
    ///
    /// The two modifiers do what they do in every list anybody has used:
    /// `ctrl` takes one row in or out, `shift` reaches from the anchor to
    /// here. Plain means this row and nothing else.
    pub fn click_row(&mut self, row: usize, toggle: bool, reach: bool) {
        if row >= self.tracks.len() {
            return;
        }
        if reach && let Some(anchor) = self.selected() {
            let (from, to) = (anchor.min(row), anchor.max(row));
            // The anchor is put back on the end so it stays the anchor: the
            // next shift-click should reach from where the reader started,
            // not from wherever the last reach happened to stop.
            let mut picked: Vec<usize> = (from..=to).filter(|it| *it != anchor).collect();
            picked.push(anchor);
            self.selection = picked;
            return;
        }
        if toggle {
            match self.selection.iter().position(|it| *it == row) {
                Some(at) => {
                    self.selection.remove(at);
                }
                None => self.selection.push(row),
            }
            return;
        }
        self.select_only(row);
    }

    /// Nothing is pointed at.
    pub fn select_none(&mut self) {
        self.selection.clear();
    }

    /// Folds a scope shut, or opens it again.
    ///
    /// The tracks under it stay on the panel: folded is a way of *looking* at
    /// the list, not a way of shortening it. A reader who wanted them gone has
    /// the × and the Delete key, and neither of those is what a triangle
    /// means anywhere else.
    pub fn fold(&mut self, path: &str) {
        if !self.shut.remove(path) {
            self.shut.insert(path.to_string());
        }
    }

    /// Whether a scope is folded shut.
    pub fn is_shut(&self, path: &str) -> bool {
        self.shut.contains(path)
    }

    /// Opens whatever is folded over a track, so it can be seen.
    ///
    /// A fold is a way of looking at the list, and it must not become a way of
    /// hiding an answer: `first difference` jumps to a row, adding a signal
    /// that is already there points at it, and both of those are the panel
    /// saying *here*. Pointing at a row inside a shut scope says nothing at
    /// all, and looks exactly like the button having done nothing.
    fn reveal(&mut self, row: usize) {
        let Some(Track::Signal { name, .. }) = self.tracks.get(row) else { return };
        let parts: Vec<&str> = name.split('.').collect();
        // Every scope on the way down, which is every prefix but the signal.
        for depth in 1..parts.len() {
            self.shut.remove(&parts[..depth].join("."));
        }
    }

    /// Picks a whole scope's worth of rows.
    ///
    /// Plain, they become the selection. With `ctrl` they join it, unless
    /// every one of them is already in — then they leave, which is what makes
    /// the same click take a module back off again.
    pub fn pick_all(&mut self, rows: &[usize], add: bool) {
        if rows.is_empty() {
            return;
        }
        let already = rows.iter().all(|row| self.selection.contains(row));
        if !add {
            self.selection = rows.to_vec();
            return;
        }
        match already {
            true => self.selection.retain(|row| !rows.contains(row)),
            false => {
                for row in rows {
                    if !self.selection.contains(row) {
                        self.selection.push(*row);
                    }
                }
            }
        }
    }

    pub fn remove(&mut self, track: usize) {
        if track < self.tracks.len() {
            self.tracks.remove(track);
            // The selection is a list of positions, and every position after
            // this one has just moved. Pointing at the wrong row is worse than
            // pointing at none.
            self.selection.retain(|it| *it != track);
            for it in &mut self.selection {
                if *it > track {
                    *it -= 1;
                }
            }
            self.forget_caches();
            self.reload();
        }
    }

    /// Takes every picked row off the panel, and says how many that was.
    ///
    /// Highest first. A list of positions is only true while nothing earlier
    /// has gone, so removing from the front would take the wrong rows from the
    /// second one on — and the rows it took would be somebody else's.
    ///
    /// One reload at the end rather than one a row: re-reading every remaining
    /// signal out of the dump for each row taken off is the same work done as
    /// many times as the reader picked.
    pub fn remove_selected(&mut self) -> usize {
        let rows = self.selected_rows();
        let removed = rows.iter().filter(|row| **row < self.tracks.len()).count();
        for row in rows.into_iter().rev() {
            if row < self.tracks.len() {
                self.tracks.remove(row);
            }
        }
        self.selection.clear();
        self.forget_caches();
        self.reload();
        removed
    }

    /// Moves rows to sit in front of the track now at `before`.
    ///
    /// The order a reader puts rows in is an argument they are making — a
    /// clock, then the valid, then the data it qualifies — and no analysis in
    /// this program can make it for them. So the list is theirs to arrange, and
    /// this is the one operation that arranges it.
    ///
    /// `before` is a position in the list *as it is now*, and `tracks.len()`
    /// means the end. The moved rows keep their order among themselves and
    /// arrive as one block, however scattered they were: a reader who picked
    /// three rows from three modules and dropped them together asked for them
    /// together.
    ///
    /// Returns whether anything actually moved, so a drop that landed where it
    /// started can say nothing rather than claim a move.
    pub fn move_rows(&mut self, rows: &[usize], before: usize) -> bool {
        let mut moving: Vec<usize> =
            rows.iter().copied().filter(|row| *row < self.tracks.len()).collect();
        moving.sort_unstable();
        moving.dedup();
        if moving.is_empty() {
            return false;
        }
        let before = before.min(self.tracks.len());

        // The new order, as the old positions in the order they will be in.
        // Built rather than swapped so that the two hard parts — a block that
        // was scattered, and a destination that moves as the rows leave — are
        // one walk with nothing to get off by one.
        let mut order: Vec<usize> = Vec::with_capacity(self.tracks.len());
        for old in 0..=self.tracks.len() {
            if old == before {
                order.extend(moving.iter().copied());
            }
            if old < self.tracks.len() && !moving.contains(&old) {
                order.push(old);
            }
        }
        if order.iter().copied().eq(0..self.tracks.len()) {
            return false;
        }

        let mut taken: Vec<Option<Track>> = self.tracks.drain(..).map(Some).collect();
        self.tracks = order
            .iter()
            .map(|old| taken[*old].take().expect("each old position is used exactly once"))
            .collect();

        // Where every old position ended up, so that anything holding one can
        // be moved with it. Getting this wrong points the keyboard, the ways
        // back into the design, and the × at whatever slid into the row the
        // reader was on — which is worse than losing the selection outright,
        // because it looks like it worked.
        let mut now = vec![0usize; order.len()];
        for (new, old) in order.iter().enumerate() {
            now[*old] = new;
        }
        for row in &mut self.selection {
            *row = now.get(*row).copied().unwrap_or(*row);
        }
        // The anchor as the workbench last heard it, likewise. A row that only
        // changed position is not a row the reader just clicked, and announcing
        // it again would pull every other view to a signal nobody chose.
        if let Some(announced) = self.announced {
            self.announced = now.get(announced).copied().or(Some(announced));
        }
        self.forget_caches();
        self.reload();
        true
    }

    /// Moves the picked rows one place up or down.
    ///
    /// What the drag does, for a hand that is already on the keyboard and for
    /// a list too long to drag across. Scattered rows gather into a block at
    /// the destination, which is what [`WaveState::move_rows`] does and the
    /// only answer that does not need a second rule for "which of them moved".
    ///
    /// A block already at the end of the list stays there rather than wrapping
    /// round: the list has two ends, and a row that leapt from one to the other
    /// would look like it had been lost.
    pub fn move_selected(&mut self, down: bool) -> bool {
        let rows = self.selected_rows();
        let (Some(first), Some(last)) = (rows.first().copied(), rows.last().copied()) else {
            return false;
        };
        let before = match down {
            // Past the row below the block: one place down, since the block
            // itself does not count as somewhere to land.
            true => last + 2,
            false => match first {
                0 => return false,
                _ => first - 1,
            },
        };
        self.move_rows(&rows, before)
    }

    /// The caches are keyed by position, so they go when positions do.
    fn forget_caches(&mut self) {
        self.changes.clear();
        if let Some(reference) = self.reference.as_mut() {
            reference.changes.clear();
        }
    }

    /// Re-reads every signal track, after the list has been reordered.
    fn reload(&mut self) {
        let vars: Vec<(usize, WaveVar)> = self
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(index, track)| match track {
                Track::Signal { var, .. } => Some((index, *var)),
                Track::Decoded { .. } | Track::Stage { .. } => None,
            })
            .collect();
        for (index, var) in vars {
            let changes: Vec<(u64, WaveValue)> =
                self.dump.changes(var).map(|iter| iter.collect()).unwrap_or_default();
            self.changes.insert(index, changes);
            self.fill_ghost(index);
        }
    }

    /// The name column: what is on each line, and where each track went.
    ///
    /// Insertion order is kept. Grouping is over *runs* of tracks that share a
    /// scope, not a sort — a reader who added `u_rx.data` then `u_tx.data`
    /// then `u_rx.valid` gets `u_rx` twice, which is a true picture of the list
    /// they built. Reordering rows to make the picture tidier would move a
    /// waveform somebody put where they wanted it.
    ///
    /// Only signals are grouped. A decoder's lane and a pipeline stage are
    /// named by this program rather than by the dump, so there is no scope
    /// above them to draw and no hierarchy in them to leave out.
    pub fn layout(&self) -> Layout {
        let mut rows: Vec<Row> = Vec::new();
        let mut line_of = vec![None; self.tracks.len()];
        if self.names == Names::Path {
            for (index, line) in line_of.iter_mut().enumerate() {
                *line = Some(rows.len());
                rows.push(Row::Track { index, depth: 0 });
            }
            return Layout { rows, line_of };
        }

        // Built whole first, folds and all, because what a scope holds is a
        // fact about the list rather than about what is on screen: a folded
        // module still answers for its signals when it is clicked.
        let mut above: Vec<String> = Vec::new();
        for (index, track) in self.tracks.iter().enumerate() {
            let mut scope: Vec<String> = match track {
                Track::Signal { name, .. } => {
                    name.split('.').map(str::to_string).collect::<Vec<_>>()
                }
                _ => Vec::new(),
            };
            // The last component is the signal, not a scope it lives in.
            scope.pop();

            // What the lines above already said still holds; the rest is new.
            let kept = scope.iter().zip(&above).take_while(|(now, was)| now == was).count();
            for (depth, label) in scope.iter().enumerate().skip(kept) {
                let path = scope[..=depth].join(".");
                rows.push(Row::Scope {
                    label: label.clone(),
                    shut: self.shut.contains(&path),
                    path,
                    depth,
                    holds: Vec::new(),
                });
            }
            rows.push(Row::Track { index, depth: scope.len() });
            above = scope;
        }

        // What each scope holds, read off the lines under it: everything until
        // a scope at the same level or shallower, which is where it ends.
        for line in 0..rows.len() {
            let Row::Scope { depth, .. } = &rows[line] else { continue };
            let depth = *depth;
            let mut holds = Vec::new();
            for below in &rows[line + 1..] {
                match below {
                    Row::Scope { depth: other, .. } if *other <= depth => break,
                    Row::Track { index, depth: other } => match *other > depth {
                        true => holds.push(*index),
                        false => break,
                    },
                    Row::Scope { .. } => {}
                }
            }
            if let Row::Scope { holds: into, .. } = &mut rows[line] {
                *into = holds;
            }
        }

        // Now the folds. A scope that is shut takes every line under it away,
        // including the scopes below it — which are shut too as far as the
        // reader is concerned, whatever they say about themselves.
        let mut kept: Vec<Row> = Vec::new();
        let mut hidden_below: Option<usize> = None;
        for row in rows {
            let depth = match &row {
                Row::Scope { depth, .. } | Row::Track { depth, .. } => *depth,
            };
            if let Some(under) = hidden_below {
                match depth > under {
                    true => continue,
                    false => hidden_below = None,
                }
            }
            if let Row::Scope { shut: true, depth, .. } = &row {
                hidden_below = Some(*depth);
            }
            if let Row::Track { index, .. } = &row {
                line_of[*index] = Some(kept.len());
            }
            kept.push(row);
        }
        Layout { rows: kept, line_of }
    }

    /// What each track is called on its own line.
    ///
    /// The whole path when the column is a flat list, and the name the scope
    /// above it gave it when the column is a tree — the lines above have
    /// already said the rest, and repeating it is what filled the column with
    /// one prefix twenty times over.
    pub fn shown_names(&self) -> Vec<String> {
        self.tracks
            .iter()
            .map(|track| {
                let name = match track {
                    Track::Signal { name, .. }
                    | Track::Decoded { name, .. }
                    | Track::Stage { name, .. } => name,
                };
                match (self.names, track) {
                    (Names::Tree, Track::Signal { .. }) => {
                        name.rsplit('.').next().unwrap_or(name).to_string()
                    }
                    _ => name.clone(),
                }
            })
            .collect()
    }

    /// Every signal the design and the dump agree on, for a picker.    /// Every signal the design and the dump agree on, for a picker.
    pub fn matched_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> =
            self.matches.matched.iter().map(|m| m.ir_name.as_str()).collect();
        names.sort_unstable();
        names
    }
}

/// What a decoder's lane holds, taken from the annotations on it.
fn lane_name(report: &DecodeReport, row: u8) -> String {
    let mut kinds: Vec<&str> =
        report.annotations.iter().filter(|a| a.row == row).map(|a| a.kind.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    kinds.truncate(3);
    if kinds.is_empty() { format!("row {row}") } else { kinds.join("/") }
}

/// The design a dump is read against, when there is one.
///
/// `None` is a recording opened on its own: dropped with no sources, or named
/// on the command line by itself. Nothing about the panel needs a design — the
/// rows, the times and the values all come out of the file — so the design is
/// what adds the second layer, the one that says which net a row is. Refusing
/// to open the file without it made the common case, "somebody sent me a VCD",
/// the one case the viewer could not do.
/// The machines come with it because naming a value is the design's job too:
/// see [`state_names`].
pub type Against<'a> = Option<(&'a rtlscope_ir::Design, &'a Flattened, &'a [Fsm])>;

impl WaveState {
    /// Opens a dump and lines it up with the design, when there is one.
    pub fn open(
        path: std::path::PathBuf,
        against: Against<'_>,
    ) -> Result<Self, rtlscope_wave::WaveError> {
        let dump = Dump::open(&path)?;
        let matches = match against {
            Some((design, flat, _)) => rtlscope_wave::match_signals(&dump, design, flat, None),
            None => MatchReport::unmatched(),
        };
        let end = dump.max_time().max(1);
        let scope = if matches.prefix.is_empty() { "the top" } else { matches.prefix.as_str() };

        // A run leaves its verdict beside its waveform, and the two are only
        // useful together: a failing test names a moment worth looking at.
        let results =
            rtlscope_tb::results::beside(&path).and_then(|at| rtlscope_tb::results::read(&at).ok());

        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let mut status = match against {
            Some(_) => format!(
                "{name}: {} of {} signal(s) matched under `{scope}`",
                matches.matched.len(),
                matches.prefix_score.1
            ),
            // No count of matches, because none was attempted. Saying "0 of 0
            // matched" would read as a design that lines up with nothing.
            None => format!(
                "{name}: {} signal(s) recorded — no design open, so no net is named",
                dump.vars().count()
            ),
        };
        if let Some(run) = &results {
            let (passed, failed, skipped) = run.counts();
            status.push_str(&format!(
                "; the run beside it: {passed} passed, {failed} failed, {skipped} skipped"
            ));
        }
        let named = match against {
            Some((design, flat, fsms)) => value_names(design, flat, fsms, &matches),
            None => HashMap::new(),
        };
        let mut state = WaveState {
            dump,
            path,
            matches,
            tracks: Vec::new(),
            decodes: Vec::new(),
            t_left: 0.0,
            refit: true,
            menu_at: None,
            band: None,
            moving: None,
            named,
            per_px: end as f64 / 800.0,
            cursor: None,
            changes: HashMap::new(),
            status,
            selection: Vec::new(),
            announced: None,
            markers: Vec::new(),
            search: String::new(),
            names: Names::default(),
            shut: HashSet::new(),
            picking: false,
            picker: None,
            reference: None,
            filter: String::new(),
            buses: Vec::new(),
            domain: None,
            cycles: None,
            stage_view: None,
            stage_window: (0, 0),
            token_flow: None,
            results,
        };
        match against {
            Some((design, ..)) => state.show_the_module(design),
            None => state.show_the_dump(),
        }
        Ok(state)
    }

    /// The same first screenful, worked out from the dump alone.
    ///
    /// [`WaveState::show_the_module`] asks the design which signals are the
    /// top module's ports. With no design there is nobody to ask, so the dump
    /// answers for itself: the shallowest scope it recorded that has variables
    /// of its own — a testbench writes `tb.clk` beside `tb.dut.…`, and the
    /// handful at the top is what somebody put there to watch.
    ///
    /// Clocks and resets first, by name, for the reason the design-led version
    /// puts them first: every other row is read against them. Guessing from a
    /// name is weaker than asking the design, and it is the only evidence a
    /// lone recording carries.
    fn show_the_dump(&mut self) {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for (path, _) in self.dump.vars() {
            let scope = match path.rfind('.') {
                Some(at) => path[..at].to_string(),
                None => String::new(),
            };
            *counts.entry(scope).or_default() += 1;
        }
        // Shallowest, then the busiest of those, then by name so two equal
        // candidates always resolve the same way. Depth because whoever wrote
        // the testbench put the signals they cared about at the top; count
        // because a dump can have more than one scope up there — a Verilator
        // run writes `$rootio` beside the module — and the one holding the
        // signals is the one worth opening on.
        let chosen = counts.into_iter().min_by_key(|(scope, count)| {
            let depth = scope.matches('.').count() + usize::from(!scope.is_empty());
            (depth, std::cmp::Reverse(*count), scope.clone())
        });
        let Some((scope, _)) = chosen else { return };

        let mut names: Vec<String> = self
            .dump
            .vars()
            .map(|(path, _)| path)
            .filter(|path| match path.rfind('.') {
                Some(at) => path[..at] == scope,
                None => scope.is_empty(),
            })
            .map(String::from)
            .collect();
        names.sort_unstable();
        let (first, rest): (Vec<String>, Vec<String>) =
            names.into_iter().partition(|path| looks_like_a_clock(path));

        for path in first.iter().chain(rest.iter()).take(TRACKS_AT_FIRST) {
            let _ = self.add_by_dump_path(path);
        }
        if !self.tracks.is_empty() {
            self.refit = true;
        }
    }

    /// Puts the module's own signals on screen, so opening a dump shows one.
    ///
    /// A waveform viewer that opens empty has failed at the only thing it is
    /// for. The reader is told 16 signals matched and then shown nothing, which
    /// reads as success and is not; finding the picker, expanding a tree and
    /// clicking one name at a time is work nobody asked to do before seeing
    /// anything at all.
    ///
    /// The ports of the module the dump was matched against, in the order the
    /// source declares them, which is the order somebody wrote them in and
    /// therefore the order they think about them. Clocks first anyway, because
    /// every other row is read against them. Everything deeper stays for the
    /// picker: a design of any size has thousands of signals and showing all of
    /// them is the same failure the other way round.
    fn show_the_module(&mut self, design: &rtlscope_ir::Design) {
        let module = design.top_module();
        let named: Vec<String> = module
            .ports
            .iter()
            .map(|port| module.net(port.net).name.clone())
            .filter(|name| self.matches.by_ir_name(name).is_some())
            .collect();

        let clocks = rtlscope_tb::clocks::plan(design, design.top);
        let is_clock = |name: &String| {
            clocks.clocks.iter().any(|clock| clock.port == *name)
                || clocks.resets.iter().any(|reset| reset.port == *name)
        };
        let (first, rest): (Vec<String>, Vec<String>) =
            named.into_iter().partition(|name| is_clock(name));

        for name in first.iter().chain(rest.iter()).take(TRACKS_AT_FIRST) {
            let _ = self.add_by_ir_name(name);
        }
        if !self.tracks.is_empty() {
            self.refit = true;
        }
    }

    /// Matches this dump against a design that has just been read again.
    ///
    /// The dump has not changed, so everything read out of *it* stays: the
    /// tracks, the cursor, where the view is, what a decoder made of it. What
    /// has to be worked out again is what the *design* said — which signal is
    /// which, and the stage rows, which are the design's claim about the dump
    /// rather than anything recorded in it.
    pub fn rebind(
        &mut self,
        design: &rtlscope_ir::Design,
        flat: &Flattened,
        fsms: &[Fsm],
    ) -> String {
        let before = self.matches.matched.len();
        self.matches = rtlscope_wave::match_signals(&self.dump, design, flat, None);
        let after = self.matches.matched.len();

        // Laid out from a pipeline the design does not necessarily have any
        // more. Dropping them is not a loss of information; keeping them would
        // be an invention.
        self.tracks.retain(|track| !matches!(track, Track::Stage { .. }));
        self.domain = None;
        self.cycles = None;
        self.stage_view = None;
        self.stage_window = (0, 0);
        self.token_flow = None;
        self.buses.clear();
        // Rebuilt with the rest: the names come from the design, so a design
        // read again can have renamed a member, dropped an enum, or given the
        // signal a different type. Keeping the old map would be showing the
        // reader a name the file no longer contains.
        self.named = value_names(design, flat, fsms, &self.matches);

        // And each row is asked again which net it is. A track keeps its data
        // — that came out of the dump and has not moved — but the net behind it
        // is the design's answer, and there is a new one. It is also the whole
        // of what sources arriving *after* a recording buy: every row put on
        // the panel before them had no net at all, and this is where they get
        // one and become clickable through to the diagram and the source.
        for track in &mut self.tracks {
            if let Track::Signal { name, signal, .. } = track {
                *signal = self.matches.by_dump_path(name).map(|matched| matched.signal);
            }
        }

        self.status = if after == before {
            format!("the dump still matches {after} signal(s)")
        } else {
            format!("the dump now matches {after} signal(s), {before} before")
        };
        self.status.clone()
    }

    /// Lays a clock domain's stages out in cycles, one track each.
    pub fn open_stages(&mut self, domain: DomainDepth) {
        let cycles =
            match rtlscope_wave::stages::cycles(&mut self.dump, &self.matches, &domain.clock) {
                Ok(cycles) => cycles,
                Err(error) => {
                    self.status = error.to_string();
                    return;
                }
            };

        // Asking twice replaces the rows rather than stacking a second copy of
        // the same pipeline on top of the first.
        self.tracks.retain(|track| !matches!(track, Track::Stage { .. }));
        for stage in &domain.stages {
            self.tracks
                .push(Track::Stage { stage: stage.index, name: format!("stage {}", stage.index) });
        }
        self.status = format!(
            "{}: {} stage(s) over {} cycle(s) of `{}`",
            domain.clock,
            domain.depth,
            cycles.len(),
            cycles.path
        );
        self.cycles = Some(cycles);
        self.domain = Some(domain);
        self.stage_view = None;
        self.stage_window = (0, 0);
        // Built now rather than on the first frame that draws it. Otherwise
        // anything that reads the occupancy without the wave view having been
        // painted — the pipeline diagram, sitting in a different tab — gets
        // nothing, and looks as though the stages had not been opened at all.
        self.build_stages(0, OPENING_WINDOW);
        // The caches are keyed by track position, which has just moved.
        self.changes.clear();
        self.reload();
    }

    /// The moment a cycle begins, once the stages have been laid out.
    pub fn cycle_time(&self, cycle: usize) -> Option<u64> {
        self.cycles.as_ref()?.at(cycle)
    }

    /// What each stage held at the cursor, from the window already built.
    ///
    /// `None` unless the stages have been laid out and the cursor is inside the
    /// window they cover — which is the honest answer, since a cycle outside it
    /// has not been read. Nothing is computed here: the pipeline diagram and
    /// the waveform then show one reading of one dump rather than two.
    pub fn stage_cells_at_cursor(&self) -> Option<StagesAt> {
        let cursor = self.cursor?;
        let cycles = self.cycles.as_ref()?;
        let view = self.stage_view.as_ref()?;
        let cycle = cycles.cycle_at(cursor)?;
        let column = cycle.checked_sub(view.first)?;

        let cells: Vec<(usize, StageCell)> = view
            .rows
            .iter()
            .filter_map(|row| row.cells.get(column).map(|cell| (row.stage, *cell)))
            .collect();
        if cells.is_empty() {
            return None;
        }
        Some((view.clock.clone(), cycle, cells))
    }

    /// Puts the cursor at a moment and slides the view so that it is on screen.
    pub fn jump_to(&mut self, time: u64, width: f32) {
        self.cursor = Some(time);
        let span = self.per_px * f64::from(width.max(80.0));
        self.t_left = (time as f64 - span / 2.0).max(0.0);
    }

    /// The stage occupancy over the cycles now on screen.
    ///
    /// Rebuilt only when the window moves by a stride rather than every frame,
    /// which is the same bargain the runs make: panning by a cycle reuses what
    /// is already there.
    fn stages_in_view(&mut self, t_right: f64) -> Option<&StageView> {
        const STRIDE: usize = 64;

        let cycles = self.cycles.as_ref()?;
        let last = cycles.len().saturating_sub(1);
        let from = cycles.cycle_at(self.t_left.max(0.0) as u64).unwrap_or(0);
        let to = cycles.cycle_at(t_right.max(0.0) as u64).unwrap_or(last);
        let first = from / STRIDE * STRIDE;
        let len = (to.saturating_sub(first) + 1 + STRIDE).min(rtlscope_wave::stages::MAX_WINDOW);

        if self.stage_view.is_none() || self.stage_window != (first, len) {
            self.build_stages(first, len);
        }
        self.stage_view.as_ref()
    }

    /// Reads one window of the stages, whatever asked for it.
    fn build_stages(&mut self, first: usize, len: usize) {
        let (Some(domain), Some(cycles)) = (self.domain.as_ref(), self.cycles.as_ref()) else {
            return;
        };
        let len = len.min(cycles.len().saturating_sub(first)).max(1);
        let layout = rtlscope_wave::Layout::window(first, len);
        self.stage_view = Some(rtlscope_wave::stages::occupancy(
            &mut self.dump,
            &self.matches,
            domain,
            cycles,
            &layout,
        ));
        self.stage_window = (first, len);
        // The tokens belong to the window that has just been replaced.
        self.token_flow = None;
    }

    /// Reads a window by its first cycle, for a view that pages through them.
    pub fn build_stage_window(&mut self, first: usize) {
        let len = self.stage_window.1.max(1);
        let last = self.cycles.as_ref().map_or(0, Cycles::len).saturating_sub(1);
        self.build_stages(first.min(last), len);
    }

    /// How many cycles the clock has in the whole dump, for paging.
    pub fn stage_cycles(&self) -> usize {
        self.cycles.as_ref().map_or(0, Cycles::len)
    }

    /// The tokens followed through the window now built.
    ///
    /// Worked out on demand and kept: an immediate-mode GUI would otherwise
    /// re-follow every token sixty times a second, and the answer only changes
    /// when the window does.
    pub fn token_flow(&mut self) -> Option<&TokenFlow> {
        let view = self.stage_view.as_ref()?;
        let window = self.stage_window;
        if self.token_flow.as_ref().is_none_or(|(built, _)| *built != window) {
            self.token_flow = Some((window, rtlscope_wave::flow::tokens(view)));
        }
        self.token_flow.as_ref().map(|(_, flow)| flow)
    }

    /// Adds a signal by the design's name for it.
    pub fn add_by_ir_name(&mut self, name: &str) -> Added {
        let Some(matched) = self.matches.by_ir_name(name) else { return Added::Missing };
        let path = matched.dump_path.clone();
        self.add_by_dump_path(&path)
    }

    /// Adds a variable by the name the *dump* knows it by.
    ///
    /// This does not ask the design for permission, and that is deliberate: a
    /// testbench's own signals are in the recording and are often exactly what
    /// a failure has to be read against. Refusing to draw them because the RTL
    /// does not declare them would be the panel deciding what may be debugged.
    /// They simply come without a `signal`, and the panel says so.
    /// Adds several at once, and reports what became of them.
    ///
    /// Refused above [`ADD_AT_ONCE`] rather than trimmed to it: a reader who
    /// asked for four hundred rows and silently got sixty-four has been given
    /// a panel that is wrong in a way nothing on it says.
    pub fn add_all(&mut self, paths: &[String]) -> String {
        if paths.len() > ADD_AT_ONCE {
            return format!(
                "that is {} signals — more than the {ADD_AT_ONCE} this will put on at once. \
                 Narrow the search, or open a scope further down.",
                paths.len()
            );
        }
        let (mut added, mut already, mut missing) = (0, 0, 0);
        for path in paths {
            match self.add_by_dump_path(path) {
                Added::New => added += 1,
                Added::Already => already += 1,
                Added::Missing => missing += 1,
            }
        }
        let mut said = format!("added {added} signal(s)");
        if already > 0 {
            let _ = write!(said, "; {already} were already on the panel");
        }
        if missing > 0 {
            let _ = write!(said, "; {missing} could not be read from the dump");
        }
        said
    }

    pub fn add_by_dump_path(&mut self, path: &str) -> Added {
        let Some(var) = self.dump.find(path) else { return Added::Missing };
        if let Some(at) = self
            .tracks
            .iter()
            .position(|track| matches!(track, Track::Signal { var: v, .. } if *v == var))
        {
            self.select_only(at);
            return Added::Already;
        }
        if self.dump.load(&[var]).is_err() {
            return Added::Missing;
        }
        let width = self.dump.width(var).unwrap_or(0);
        let signal = self.matches.by_dump_path(path).map(|matched| matched.signal);
        let index = self.tracks.len();
        let changes: Vec<(u64, WaveValue)> =
            self.dump.changes(var).map(|iter| iter.collect()).unwrap_or_default();
        self.changes.insert(index, changes);
        self.tracks.push(Track::Signal { var, name: path.to_string(), width, signal });
        self.fill_ghost(index);
        Added::New
    }

    /// Reads the reference's copy of a track, in this dump's ticks.
    ///
    /// Done here rather than at draw time because the conversion is per change
    /// and the answer never moves: a frame that recomputed it would be paying
    /// for the same arithmetic sixty times a second.
    fn fill_ghost(&mut self, index: usize) {
        let Some(Track::Signal { name, .. }) = self.tracks.get(index) else { return };
        let name = name.clone();
        let Some(reference) = self.reference.as_mut() else { return };
        let Some(var) = reference.dump.find(&name) else {
            reference.changes.remove(&index);
            return;
        };
        if reference.dump.load(&[var]).is_err() {
            return;
        }
        let convert = reference.convert;
        let changes: Vec<(u64, WaveValue)> = match reference.dump.changes(var) {
            Ok(iter) => iter.collect(),
            Err(_) => return,
        };
        let changes = match convert {
            false => changes,
            true => changes
                .into_iter()
                .filter_map(|(at, value)| {
                    let ns = reference.dump.ns_of_ticks(at)?;
                    Some((self.dump.ticks_of_ns(ns)?, value))
                })
                .collect(),
        };
        // Re-borrowed, because the conversion above needed this dump too.
        if let Some(reference) = self.reference.as_mut() {
            reference.changes.insert(index, changes);
        }
    }

    /// Adds a net picked from the block diagram, if the dump has it.
    pub fn add_net(
        &mut self,
        design: &rtlscope_ir::Design,
        flat: &Flattened,
        net: NetId,
        path: &str,
    ) {
        let module_net = design;
        let _ = module_net;
        // The design knows this net by its instance path; the dump knows it by
        // the same name with the dump's own prefix in front, which the match
        // report already resolved.
        let node = flat.nodes.iter().find(|node| node.path == path && node.signal(net).is_some());
        let Some(node) = node else {
            self.status = format!("`{path}` is not a place this dump reaches");
            return;
        };
        let module = &design.modules[node.module];
        let name = if path.is_empty() {
            module.net(net).name.clone()
        } else {
            format!("{path}.{}", module.net(net).name)
        };
        self.status = match self.add_by_ir_name(&name) {
            Added::New => format!("added {name}"),
            Added::Already => format!("{name} is already on the panel"),
            Added::Missing => format!("`{name}` is not in this dump"),
        };
    }

    /// Measures how long a value took, over the recording this panel holds.
    ///
    /// On the panel rather than in the workbench because the matching and the
    /// clock's edges are already here: asking the dump a second time would be
    /// a second chance to resolve a name differently.
    pub fn measure_latency(
        &mut self,
        clock: &str,
        from: &str,
        to: &str,
    ) -> Result<rtlscope_wave::LatencyReport, String> {
        let cycles = rtlscope_wave::stages::cycles(&mut self.dump, &self.matches, clock)
            .map_err(|error| error.to_string())?;
        rtlscope_wave::latency::latency(&mut self.dump, &self.matches, &cycles, from, to)
            .map_err(|error| error.to_string())
    }

    /// The moment the selected signal next changes, in either direction.
    ///
    /// This is what a long dump is navigated by. Scrolling to find the one
    /// moment a signal moved, in a recording of a million, is not navigation.
    pub fn edge(&self, track: usize, from: u64, forward: bool) -> Option<u64> {
        let changes = self.changes.get(&track)?;
        match forward {
            true => changes.iter().find(|(at, _)| *at > from).map(|(at, _)| *at),
            false => changes.iter().rev().find(|(at, _)| *at < from).map(|(at, _)| *at),
        }
    }

    /// The moment the selected signal next holds a value.
    ///
    /// The point of a waveform is usually one moment in it, and the reader
    /// often knows the *value* at that moment rather than the time.
    pub fn find_value(&self, track: usize, after: u64, wanted: &Wanted) -> Option<u64> {
        let changes = self.changes.get(&track)?;
        changes.iter().find(|(at, value)| *at > after && wanted.matches(value)).map(|(at, _)| *at)
    }

    /// What a signal held at the cursor, whether or not it has a row.
    ///
    /// The panel's own readout goes through `changes`, which only a track has.
    /// This asks the dump itself, so a view can show the value of something the
    /// reader never added — the state register behind an FSM diagram, which
    /// they are looking at in another pane and should not have to build a row
    /// to read.
    ///
    /// `&mut` because a dump loads a variable the first time it is asked for
    /// one; asking again costs nothing.
    pub fn value_at_cursor(&mut self, signal: SignalId) -> Option<WaveValue> {
        let at = self.cursor?;
        let var = self.matches.var_of(signal)?;
        self.dump.load(&[var]).ok()?;
        self.dump.value_at(var, at).ok().flatten()
    }

    /// When a signal next holds this value, after a moment.
    ///
    /// [`Self::find_value`] answers this for a track the reader has selected.
    /// This answers it for a signal they have not added, which is what "when is
    /// this state next entered" needs.
    pub fn next_time_holding(&mut self, signal: SignalId, after: u64, value: u64) -> Option<u64> {
        let var = self.matches.var_of(signal)?;
        self.dump.load(&[var]).ok()?;
        self.dump
            .changes(var)
            .ok()?
            .find(|(at, held)| *at > after && held.as_u64() == Some(value))
            .map(|(at, _)| at)
    }

    /// Puts a marker at the cursor, or takes away the one already there.
    pub fn toggle_marker(&mut self, at: u64) {
        match self.markers.iter().position(|marker| *marker == at) {
            Some(index) => {
                self.markers.remove(index);
            }
            None => {
                self.markers.push(at);
                self.markers.sort_unstable();
            }
        }
    }

    /// Slides one marker to another moment.
    ///
    /// Dragged onto another it merges with it rather than sitting behind it:
    /// two flags at one moment measure nothing, and one of them would be
    /// invisible and still in the way.
    pub fn move_marker(&mut self, index: usize, to: u64) {
        let Some(marker) = self.markers.get_mut(index) else { return };
        *marker = to;
        self.markers.sort_unstable();
        self.markers.dedup();
    }

    pub fn markers(&self) -> &[u64] {
        &self.markers
    }

    /// A stretch of time, in the words a reader measures one in.
    ///
    /// Nanoseconds when the dump says what a tick is worth, and enough decimal
    /// places to keep a short span from reading as zero — a picosecond dump of a
    /// gigahertz clock has half periods well under a nanosecond, and `Δ 0 ns` is
    /// worse than no answer.
    fn span_words(&self, span: u64) -> String {
        match self.dump.ns_of_ticks(span) {
            Some(ns) if ns >= 1.0 => format!("Δ {ns:.0} ns"),
            Some(ns) => format!("Δ {ns:.3} ns"),
            None => format!("Δ {span} tick(s)"),
        }
    }

    /// The distance from the cursor to the nearest marker, in words.
    ///
    /// In the dump's own units, and in cycles too when the stages have been
    /// laid out — because "seventeen cycles" is the answer to a question about
    /// a protocol, where "three hundred and forty nanoseconds" is the answer to
    /// a question about a clock.
    pub fn measure(&self) -> Option<String> {
        let cursor = self.cursor?;
        let nearest = *self.markers.iter().min_by_key(|marker| marker.abs_diff(cursor))?;
        let span = cursor.abs_diff(nearest);

        let mut said = self.span_words(span);
        if let Some(cycles) = &self.cycles
            && let (Some(here), Some(there)) = (cycles.cycle_at(cursor), cycles.cycle_at(nearest))
        {
            let _ = write!(said, " ({} cycle(s))", here.abs_diff(there));
        }
        Some(said)
    }

    /// Fits the whole dump into the width given.
    /// Opens a second recording and works out where the two part.
    ///
    /// Synchronous, like opening the first one: the budget in
    /// [`rtlscope_wave::compare`] is what bounds the cost, and a progress bar for
    /// something that finishes in the time a dump takes to open would be more
    /// machinery than the wait is worth.
    pub fn open_reference(&mut self, path: std::path::PathBuf) -> Result<(), WaveError> {
        let mut dump = Dump::open(&path)?;
        let comparison = rtlscope_wave::compare(&mut self.dump, &mut dump);
        let differing: HashSet<String> =
            comparison.differing.iter().map(|one| one.path.clone()).collect();
        let convert = match (self.dump.timescale(), dump.timescale()) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        };

        self.status = match comparison.first() {
            None if comparison.shared == 0 => {
                "these two recordings have no signal in common".to_string()
            }
            None => format!("{} shared signal(s), and none of them differ", comparison.shared),
            Some(first) => format!(
                "{} of {} signal(s) differ; the first is `{}` at {}",
                comparison.differing.len(),
                comparison.shared,
                first.path,
                first.at
            ),
        };
        self.reference =
            Some(Reference { path, dump, comparison, differing, changes: HashMap::new(), convert });
        self.reload();
        Ok(())
    }

    /// Forgets the second recording.
    pub fn drop_reference(&mut self) {
        self.reference = None;
        self.status = "comparison closed".to_string();
    }

    /// Puts the cursor on the first moment the two recordings part, on the
    /// track it happened on — adding that track when it is not being shown,
    /// since the answer is no use without the row it is about.
    pub fn seek_first_difference(&mut self, width: f32) {
        let Some(first) =
            self.reference.as_ref().and_then(|it| it.comparison.differing.first()).cloned()
        else {
            self.status = "nothing to go to: no second recording is open".to_string();
            return;
        };
        if !self.add_by_dump_path(&first.path).shown() {
            self.status = format!("`{}` could not be read from this dump", first.path);
            return;
        }
        // The row it happened on, alone: this is an answer to one
        // question, and leaving anything the reader had picked selected
        // beside it would make the panel say two things at once.
        self.selection = self
            .tracks
            .iter()
            .position(|track| matches!(track, Track::Signal { name, .. } if *name == first.path))
            .into_iter()
            .collect();
        self.jump_to(first.at, width);
        self.cursor = Some(first.at);
        self.status =
            format!("`{}` at {}: {} here, {} there", first.path, first.at, first.a, first.b);
    }

    /// Opens the signal picker, with something already typed into it.
    pub fn open_picker(&mut self, needle: &str) {
        self.picking = true;
        needle.clone_into(&mut self.filter);
    }

    /// Keeps the view over the recording.
    ///
    /// Nothing used to. The wheel could zoom out until the dump was a bright
    /// line in a field of empty grid, and a pan could leave it behind
    /// altogether — a panel showing a thousand times more time than was ever
    /// recorded, with no hint of which way to go back.
    ///
    /// A quarter of the recording is allowed past its end, and no more. That
    /// much is useful: the last edge should not sit against the frame, and a
    /// gap that says "this is where it stops" is worth a corner of the screen.
    /// Beyond that there is nothing to see, so there is no reason to go.
    ///
    /// Applied after every gesture rather than inside each one, because the
    /// gestures compose — a wheel during a drag, a band released while zoomed
    /// out — and one invariant enforced once cannot disagree with itself.
    fn hold_in_view(&mut self, width: f32) {
        let end = self.dump.max_time().max(1) as f64;
        let widest = end * 1.25;
        let width = f64::from(width.max(1.0));

        self.per_px = self.per_px.clamp(f64::MIN_POSITIVE, widest / width);
        let shown = self.per_px * width;
        self.t_left = self.t_left.clamp(0.0, (widest - shown).max(0.0));
    }

    /// Puts one stretch of time across the whole plot.
    ///
    /// The caller passes the width for the same reason [`WaveState::fit`] and
    /// [`WaveState::jump_to`] do: this does not know how wide it is drawn until
    /// egui has laid it out. What the ends mean is [`zoomed`]'s to decide.
    pub fn zoom_to(&mut self, from: f64, to: f64, width: f32) {
        if let Some((left, per_px)) = zoomed(from, to, width) {
            self.t_left = left;
            self.per_px = per_px;
        }
    }

    pub fn fit(&mut self, width: f32) {
        self.t_left = 0.0;
        self.per_px = (self.dump.max_time().max(1) as f64) / f64::from(width.max(1.0));
    }

    fn runs_for(&self, layer: Layer, track: usize, width: f32) -> Vec<Run> {
        let empty = Vec::new();
        let changes = match layer {
            Layer::Recorded => self.changes.get(&track).unwrap_or(&empty),
            Layer::Reference => {
                self.reference.as_ref().and_then(|it| it.changes.get(&track)).unwrap_or(&empty)
            }
        };
        // `self.per_px`, not the quantised scale the bins are built at. That
        // quantisation is why this had to be written: rounding the scale down
        // to a power of two makes each bin cover more time than its pixel does,
        // so a clock lands two edges in one bin, is called too dense to draw,
        // and comes out as a row of ticks. Runs are not cached, so they can be
        // built at the scale actually on screen.
        runs(changes, self.t_left, self.per_px.max(f64::MIN_POSITIVE), width)
    }

    /// What one recording's copy of a track held at the cursor.
    ///
    /// `None` when there is no cursor, no such track, or — for the reference —
    /// no second recording: three different absences, all of which come to the
    /// same thing for a caller that just wants a number to print.
    fn value_of(&self, layer: Layer, track: usize) -> Option<String> {
        let at = self.cursor?;
        let changes = match layer {
            Layer::Recorded => self.changes.get(&track)?,
            Layer::Reference => self.reference.as_ref()?.changes.get(&track)?,
        };
        let index = changes.partition_point(|(time, _)| *time <= at).checked_sub(1)?;
        // Both layers go through the same spelling, because the readout beside
        // the row is red when they differ and that comparison is on the string.
        // Two spellings of one value would report a difference that is not one.
        changes.get(index).map(|(_, value)| self.shown_for(track, value))
    }

    /// What one signal's enum type calls this value, or the number.
    fn shown_for(&self, track: usize, value: &WaveValue) -> String {
        match self.names_for(track).zip(value.as_u64()) {
            Some((names, number)) => i64::try_from(number)
                .ok()
                .and_then(|number| names.get(&number))
                .cloned()
                .unwrap_or_else(|| shown(value)),
            None => shown(value),
        }
    }

    /// What this track's values are called, if anything calls them anything.
    fn names_for(&self, track: usize) -> Option<&HashMap<i64, String>> {
        match self.tracks.get(track) {
            Some(Track::Signal { signal: Some(signal), .. }) => self.named.get(signal),
            _ => None,
        }
    }

    /// What the reader typed, read as a value of the selected signal.
    ///
    /// A state name is resolved *before* [`Wanted::parse`] rather than added to
    /// it as a third spelling. `parse` treats anything holding an `x` or a `z`
    /// as a bit pattern, which is right for `0bxx` and wrong for `IDLE_TX`; and
    /// a name is a fact about one signal, which `parse` has no way to know.
    fn wanted_for(&self, track: usize, text: &str) -> Option<Wanted> {
        let named = self.names_for(track).and_then(|names| {
            let wanted = text.trim();
            names
                .iter()
                .find(|(_, name)| name.as_str() == wanted)
                .and_then(|(value, _)| u64::try_from(*value).ok())
                .map(Wanted::Number)
        });
        named.or_else(|| Wanted::parse(text))
    }

    fn x_of(&self, time: u64, left: f32) -> f32 {
        left + ((time as f64 - self.t_left) / self.per_px) as f32
    }

    /// The time under a pixel, unrounded and unclamped.
    ///
    /// [`WaveState::time_at`] floors at zero and truncates to a tick, which is
    /// right for a cursor — it lands on a moment the dump has. A band is
    /// arithmetic: rounding both ends before subtracting makes a narrow one
    /// collapse, and clamping the anchor makes a drag leftwards off the start
    /// of time zoom to somewhere nobody pointed at.
    fn time_at_raw(&self, x: f32, left: f32) -> f64 {
        self.t_left + f64::from(x - left) * self.per_px
    }

    fn time_at(&self, x: f32, left: f32) -> u64 {
        (self.t_left + f64::from(x - left) * self.per_px).max(0.0) as u64
    }
}

/// A value to look for, as the reader wrote it.
///
/// Three spellings, because a reader looking for a bus value thinks in
/// hexadecimal, one looking at a counter thinks in decimal, and one chasing an
/// `x` cannot write it as either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wanted {
    Number(u64),
    /// A bit pattern, matched against the value's bits. This is the only way to
    /// ask for `x` or `z`, which are not numbers.
    Bits(String),
}

impl Wanted {
    /// Reads what was typed, or `None` if it is not a value at all.
    ///
    /// `0x1c` and `0b11100` say their own base. A bare number is decimal,
    /// because that is what a bare number means everywhere else a person
    /// writes one. Anything holding an `x` or a `z` is a bit pattern.
    pub fn parse(text: &str) -> Option<Wanted> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let lower = text.to_ascii_lowercase();
        if let Some(digits) = lower.strip_prefix("0x") {
            return u64::from_str_radix(digits, 16).ok().map(Wanted::Number);
        }
        if let Some(digits) = lower.strip_prefix("0b") {
            return match digits.contains(['x', 'z']) {
                true => Some(Wanted::Bits(digits.to_string())),
                false => u64::from_str_radix(digits, 2).ok().map(Wanted::Number),
            };
        }
        if lower.contains(['x', 'z']) {
            return Some(Wanted::Bits(lower));
        }
        lower.parse::<u64>().ok().map(Wanted::Number)
    }

    fn matches(&self, value: &WaveValue) -> bool {
        match self {
            Wanted::Number(number) => value.as_u64() == Some(*number),
            // Against the low bits, so `x1` finds a wide bus ending in `x1`
            // without the reader writing out the leading bits they do not know.
            Wanted::Bits(bits) => {
                let held = value.bit_string().to_ascii_lowercase();
                held.len() >= bits.len() && held.ends_with(bits.as_str())
            }
        }
    }
}

/// What the panel wants the app to do, since the app owns the design.
#[derive(Debug, Default)]
pub struct WaveAction {
    /// Propose the buses on the module being looked at.
    pub find_buses: bool,
    /// Run this suggestion, by its position in the last proposal.
    pub decode: Option<usize>,
    /// Lay the pipeline out in cycles, which needs the design.
    pub find_stages: bool,
    pub close: bool,
    /// Show the selected track's wire in the block diagram.
    pub show_net: Option<SignalId>,
    /// Show where it is declared.
    pub show_source: Option<SignalId>,
    /// Ask where its value comes from.
    pub trace: Option<SignalId>,
    /// Take the next dump that arrives as the one to compare against.
    pub want_reference: bool,
    /// The selection moved to this wire.
    ///
    /// Not the same as [`WaveAction::show_source`], which is a button being
    /// pressed and means "take me there". This is the reader looking at a row,
    /// and the views that can follow quietly should — without taking the panel
    /// off whatever they had open to do it.
    pub followed: Option<SignalId>,
}

/// Draws the wave view into the space it is given.
///
/// The tab around it belongs to the dock: this used to own an `egui::Panel` of
/// its own, and a view that brings its own frame cannot be put behind a tab.
pub fn show(ui: &mut Ui, state: &mut WaveState) -> WaveAction {
    let mut action = WaveAction::default();
    {
        ui.horizontal(|ui| {
            if ui.button("fit").on_hover_text("Show the whole dump").clicked() {
                state.refit = true;
            }
            if ui
                .selectable_label(state.names == Names::Tree, "hierarchy")
                .on_hover_text(
                    "Draw the scopes the signals live in, and put each signal under its \
                     own — instead of one flat list of whole paths. The selected row's \
                     full path is always under this bar.",
                )
                .clicked()
            {
                state.names = match state.names {
                    Names::Tree => Names::Path,
                    Names::Path => Names::Tree,
                };
            }
            if ui.button("signals…").clicked() {
                state.picking = !state.picking;
            }
            if ui.button("decode…").on_hover_text("Find the buses on this module").clicked() {
                action.find_buses = true;
                state.picking = false;
            }
            if ui
                .button("stages…")
                .on_hover_text("Lay the pipeline's stages out cycle by cycle")
                .clicked()
            {
                action.find_stages = true;
                state.picking = false;
            }
            match state.reference.is_some() {
                false => {
                    if ui
                        .button("compare…")
                        .on_hover_text("Hold this recording against another")
                        .clicked()
                    {
                        action.want_reference = true;
                        state.picking = false;
                    }
                }
                true => {
                    if ui.button("first difference").clicked() {
                        let width = (ui.available_width() - NAME_WIDTH).max(80.0);
                        state.seek_first_difference(width);
                    }
                    if ui.button("uncompare").clicked() {
                        state.drop_reference();
                    }
                }
            }
            if ui.button("close").clicked() {
                action.close = true;
            }

            ui.separator();
            ui.label("find");
            let box_width = 90.0;
            let entry = ui.add(
                egui::TextEdit::singleline(&mut state.search)
                    .desired_width(box_width)
                    .hint_text("0x1c"),
            );
            let asked = entry.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (asked || ui.button("go").on_hover_text(FIND_HELP).clicked())
                && !state.search.trim().is_empty()
            {
                let width = ui.available_width().max(80.0);
                match state.selected() {
                    // Read against the selected signal, because a state name is
                    // only a value of the signal that was declared with it.
                    Some(track) => match state.wanted_for(track, &state.search) {
                        Some(wanted) => {
                            let from = state.cursor.unwrap_or(0);
                            match state.find_value(track, from, &wanted) {
                                Some(at) => state.jump_to(at, width),
                                None => {
                                    state.status =
                                        format!("`{}` does not come up after here", state.search)
                                }
                            }
                        }
                        None => {
                            state.status =
                                format!("`{}` is not a value this signal can hold", state.search)
                        }
                    },
                    None => {
                        state.status =
                            "click a signal's name first — a search is of one signal".to_string()
                    }
                }
            }

            // What the cursor and the nearest marker are apart, which is the
            // question a protocol's latency is asked as.
            if let Some(measured) = state.measure() {
                ui.separator();
                ui.label(egui::RichText::new(measured).monospace().strong());
            }

            ui.separator();
            ui.label(egui::RichText::new(&state.status).weak());
        });

        // What the run came to, and a way to the moment each failure happened.
        let mut jump: Option<f64> = None;
        if let Some(results) = &state.results {
            let (passed, failed, skipped) = results.counts();
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "tests: {passed} passed, {failed} failed, {skipped} skipped"
                    ))
                    .weak(),
                );
                for test in results.failures() {
                    let hover = match &test.message {
                        Some(message) => format!("{}\n\n{message}", test.full_name()),
                        None => test.full_name(),
                    };
                    // A multiplication sign rather than a ballot cross: the
                    // latter is not in egui's fonts and draws as tofu.
                    if ui.button(format!("× {}", test.name)).on_hover_text(hover).clicked() {
                        jump = Some(test.end_ns);
                    }
                }
            });
        }
        if let Some(ns) = jump {
            let width = ui.available_width() - NAME_WIDTH;
            match state.dump.ticks_of_ns(ns) {
                Some(tick) => {
                    state.jump_to(tick, width);
                    state.status = format!("{ns} ns is tick {tick} of this dump");
                }
                None => {
                    state.status = "this dump declares no timescale, so a moment given in \
                                    nanoseconds cannot be found in it"
                        .to_string();
                }
            }
        }

        // The buses the app last proposed, one button each.
        if !state.buses.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("buses:").weak());
                for (index, suggestion) in state.buses.iter().enumerate() {
                    let label = format!("{} {}", suggestion.protocol, suggestion.group);
                    let hover: Vec<String> = suggestion
                        .bindings
                        .iter()
                        .map(|b| {
                            format!("{}={}{}", b.role, if b.invert { "!" } else { "" }, b.path)
                        })
                        .collect();
                    if ui.button(label).on_hover_text(hover.join("\n")).clicked() {
                        action.decode = Some(index);
                    }
                }
            });
        }

        reference_row(ui, state);
        selected_row(ui, state, &mut action);

        if state.picking {
            signal_picker(ui, state);
        }
        ui.separator();

        // Reserved now and painted after the tracks: a time axis has to line up
        // with the plot, and where the plot starts is only known once the
        // scroll area has laid itself out. Pinned above rather than scrolling
        // with the rows, because an axis that scrolls off is an axis nobody can
        // read a position from.
        let (ruler_at, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), RULER_HEIGHT), Sense::hover());

        if state.tracks.is_empty() {
            // Never a blank grid. Either something is shown, or the reason
            // nothing is and what to press about it.
            crate::views::empty_state(
                ui,
                "no signals on screen",
                "Press `signals…` to pick some, or click a wire in the diagram.",
            );
            return action;
        }

        let mut geometry = None;
        // The wheel means two things, and which one it means is where the
        // pointer is: over the names it moves the list, because that is a list
        // and that is what a wheel does to one, and over the plot it zooms
        // about the pointer, which is what this panel was built around. The
        // scroll area is told not to take the wheel in the second case —
        // otherwise one notch both scrolled the rows and changed the scale,
        // and neither movement was the one asked for.
        let source = match wheel_moves_the_list(pointer(ui), ui.max_rect().left()) {
            true => egui::containers::scroll_area::ScrollSource::default(),
            false => egui::containers::scroll_area::ScrollSource {
                mouse_wheel: false,
                ..egui::containers::scroll_area::ScrollSource::default()
            },
        };
        egui::ScrollArea::vertical().id_salt("wave-tracks").scroll_source(source).show(ui, |ui| {
            let available = ui.available_width();
            let plot_width = (available - NAME_WIDTH).max(80.0);
            let rows = state.layout().lines().max(1);
            // An id of its own, rather than the one its position in the panel
            // would give it. What sits above this comes and goes — the row of
            // buttons for the selected track appears the moment anything is
            // picked — and a widget whose id moves mid-gesture loses the
            // gesture: egui keys a drag by the id that started it.
            //
            // Measured: a drag that picked up a row was delivered for exactly
            // one frame. The next frame had one more line of toolbar above it,
            // so this was a different widget, and neither the rest of the drag
            // nor its release ever arrived — the rows were put down where they
            // had been picked up, which looks exactly like a feature that does
            // nothing.
            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(available, TRACK_HEIGHT * rows as f32 + 4.0),
                Sense::hover(),
            );
            let response =
                ui.interact(rect, ui.id().with("wave-tracks-body"), Sense::click_and_drag());
            let plot_left = rect.left() + NAME_WIDTH;

            // Now that the plot has a width, before anything is drawn against
            // the scale it sets.
            if state.refit {
                state.fit(plot_width);
                state.refit = false;
            }

            handle_input(ui, state, &response, plot_left, plot_width);
            marker_menu(state, &response);
            draw(ui, state, rect, plot_left, plot_width);
            remove_buttons(ui, state, rect);
            geometry = Some((plot_left, plot_width));
        });
        // Under the rows rather than pinned to the bottom of the window: a bar
        // floating below a short list of tracks reads as belonging to the empty
        // space, not to the rows it moves.
        let (bar_at, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), SCROLL_HEIGHT), Sense::hover());
        if let Some((plot_left, plot_width)) = geometry {
            ruler(ui, state, ruler_at, plot_left, plot_width);
            scrollbar(ui, state, bar_at, plot_left, plot_width);
        }
    }
    // After everything that could have moved it: a click on a name, an arrow
    // key, a track removed out from under it.
    action.followed = newly_selected(state);
    action
}

/// How faint the reference is drawn behind the recording.
const GHOST: f32 = 0.34;

/// What the second recording is, and what it came to.
///
/// Above the tracks rather than in the status line, because it is a state the
/// panel is in: every `!=` on a row means nothing without it.
fn reference_row(ui: &mut Ui, state: &WaveState) {
    let theme = Theme::of(ui);
    let Some(reference) = &state.reference else { return };
    let name = reference.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let report = &reference.comparison;

    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("against").weak());
        ui.label(egui::RichText::new(name).monospace());
        let (differ, shared) = (report.differing.len(), report.shared);
        let text = match report.first() {
            None if shared == 0 => "no signal in common".to_string(),
            None => format!("{shared} shared, none differ"),
            Some(first) => format!("{differ} of {shared} differ, first at {}", first.at),
        };
        let colour = match report.agrees() {
            true => theme.ok,
            false => theme.err,
        };
        ui.label(egui::RichText::new(text).color(colour));

        for (label, only) in [("only here", &report.only_in_a), ("only there", &report.only_in_b)] {
            if !only.is_empty() {
                let listed = only.iter().take(20).cloned().collect::<Vec<_>>().join("\n");
                ui.label(egui::RichText::new(format!("{label}: {}", only.len())).weak())
                    .on_hover_text(listed);
            }
        }
        // Said out loud rather than left for the reader to wonder about.
        for problem in &report.problems {
            ui.label(egui::RichText::new("!").color(theme.warn)).on_hover_text(problem);
        }
    });
}

/// The selected track, and the ways back into the design from it.
///
/// Only for a signal the design was matched to: a testbench's own variable has
/// no module to show, no line to open and no driver to name, and three buttons
/// that would say so one at a time are worse than the sentence that says it
/// once.
///
/// With several rows picked it says so instead, and offers the one thing that
/// is about several rows. The three buttons are not among them: a declaration
/// and a wire in the diagram belong to one signal, and offering them for six
/// would be offering to answer a question nobody can have asked.
fn selected_row(ui: &mut Ui, state: &mut WaveState, action: &mut WaveAction) {
    let picked = state.selected_rows();
    if picked.len() > 1 {
        let mut take = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(format!("{} rows picked", picked.len())).strong());
            take = ui.small_button("take off the panel").on_hover_text("Or press Delete").clicked();
            if ui.small_button("none").on_hover_text("Pick none of them").clicked() {
                state.select_none();
            }
        });
        if take {
            let taken = state.remove_selected();
            state.status = format!("took {taken} rows off the panel");
        }
        return;
    }
    let Some(index) = state.selected() else { return };
    let Some(Track::Signal { name, signal, .. }) = state.tracks.get(index) else { return };
    let (name, signal) = (name.clone(), *signal);

    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(&name).monospace());
        let Some(signal) = signal else {
            ui.label(egui::RichText::new("is in the dump but not in the design").weak());
            return;
        };
        if ui.small_button("diagram").on_hover_text("Show this wire in the block diagram").clicked()
        {
            action.show_net = Some(signal);
        }
        if ui.small_button("source").on_hover_text("Where it is declared").clicked() {
            action.show_source = Some(signal);
        }
        if ui.small_button("trace").on_hover_text("Where its value comes from").clicked() {
            action.trace = Some(signal);
        }
    });
}

/// The ruler's ticks, carried down through the tracks.
///
/// Same step as [`ruler`], deliberately from the same arithmetic: a grid that
/// disagrees with the numbers above it is worse than no grid.
fn grid(
    painter: &egui::Painter,
    state: &WaveState,
    rect: Rect,
    plot_left: f32,
    plot_width: f32,
    rule: Color32,
) {
    let step = nice_step(state.per_px * f64::from(TICK_SPACING));
    if step <= 0.0 {
        return;
    }
    let stroke = Stroke::new(1.0, rule);
    let mut at = (state.t_left / step).ceil() * step;
    let last = state.t_left + state.per_px * f64::from(plot_width);
    while at <= last {
        let x = state.x_of(at as u64, plot_left);
        if x >= plot_left {
            // Dotted by hand: egui paints solid lines, and a solid one this
            // tall reads as something in the recording rather than as a rule.
            let mut y = rect.top();
            while y < rect.bottom() {
                painter.line_segment(
                    [Pos2::new(x, y), Pos2::new(x, (y + 2.0).min(rect.bottom()))],
                    stroke,
                );
                y += 5.0;
            }
        }
        at += step;
    }
}

/// What the find box takes, said where it is asked for.
const FIND_HELP: &str = "The next moment the selected signal holds this: `0x1c`, `28`, \
                         `0b11100`, a bit pattern like `x1` for one holding x, or \
                         a member name like `S_RUN` where the signal has an enum type.";

/// The bar along the bottom: where in the recording the plot is looking.
///
/// The waveform already pans four ways — shift-drag, a drag begun in the name
/// column, the arrow keys, the wheel about the pointer — and none of them says
/// *where* the view is. A recording is a fixed length and a plot shows a slice
/// of it; a bar is the one control that draws that relationship instead of
/// leaving it to be inferred from the ruler's numbers.
///
/// It scrolls and never zooms. Every other gesture on this panel changes the
/// scale, so the one that does not is worth having.
fn scrollbar(ui: &mut Ui, state: &mut WaveState, rect: Rect, plot_left: f32, plot_width: f32) {
    let groove = Rect::from_min_max(
        Pos2::new(plot_left, rect.top() + 3.0),
        Pos2::new(plot_left + plot_width, rect.bottom() - 3.0),
    );
    let response = ui.interact(groove, ui.id().with("wave-scrollbar"), Sense::click_and_drag());
    let bar = Bar::of(state.per_px, plot_width, state.dump.max_time(), groove.width());

    if bar.scrolls() {
        // A press away from the thumb jumps first and drags after, which is how
        // every other scrollbar behaves and therefore what a hand expects.
        if (response.drag_started() || response.clicked())
            && let Some(pos) = response.interact_pointer_pos()
        {
            let along = pos.x - groove.left();
            let at = bar.offset(state.t_left);
            if along < at || along > at + bar.thumb {
                state.t_left = bar.centred(along, groove.width());
            }
        }
        if response.dragged() {
            let moved = bar.offset(state.t_left) + response.drag_delta().x;
            state.t_left = bar.t_left(moved);
        }
    }

    let theme = Theme::of(ui);
    let painter = ui.painter_at(rect);
    painter.rect_filled(groove, 3.0, theme.surface_alt);

    // The marks are a map of the recording rather than decoration: a marker
    // dropped and then scrolled away from is otherwise gone, and this says
    // which way to go back to it.
    let along = |time: u64| groove.left() + (time as f64 / bar.span) as f32 * groove.width();
    for marker in &state.markers {
        let x = along(*marker);
        painter.line_segment(
            [Pos2::new(x, groove.top() + 1.0), Pos2::new(x, groove.bottom() - 1.0)],
            Stroke::new(1.0, theme.accent.gamma_multiply(0.55)),
        );
    }
    if let Some(cursor) = state.cursor {
        let x = along(cursor);
        painter.line_segment(
            [Pos2::new(x, groove.top() + 1.0), Pos2::new(x, groove.bottom() - 1.0)],
            Stroke::new(1.0, theme.err),
        );
    }

    let at = groove.left() + bar.offset(state.t_left);
    let thumb =
        Rect::from_min_max(Pos2::new(at, groove.top()), Pos2::new(at + bar.thumb, groove.bottom()));
    let lit = response.hovered() || response.dragged();
    painter.rect_filled(thumb, 3.0, theme.accent_soft.gamma_multiply(if lit { 1.0 } else { 0.7 }));
    painter.rect_stroke(
        thumb,
        3.0,
        Stroke::new(1.0, if lit { theme.accent } else { theme.line }),
        egui::StrokeKind::Inside,
    );
    if bar.scrolls() {
        response.on_hover_text("Drag to move through the recording; the scale does not change");
    }
}

/// The time axis, and the moments marked on it.
///
/// Everything here is in the dump's own ticks; the labels are nanoseconds when
/// the dump declared a timescale and ticks when it did not, because a number
/// with the wrong unit on it is worse than a number with none.
fn ruler(ui: &mut Ui, state: &mut WaveState, rect: Rect, plot_left: f32, plot_width: f32) {
    let theme = Theme::of(ui);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme.surface_alt);
    painter.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(0.5, theme.line));

    let unit = match state.dump.timescale() {
        Some(_) => "ns",
        None => "ticks",
    };
    painter.text(
        Pos2::new(rect.left() + 4.0, rect.center().y),
        Align2::LEFT_CENTER,
        unit,
        FontId::monospace(9.0),
        theme.muted,
    );

    // Round numbers, so a position can be read off between two of them. Ticks
    // at 137, 274, 411 are a ruler nobody can use.
    let step = nice_step(state.per_px * f64::from(TICK_SPACING));
    let first = (state.t_left / step).ceil() * step;
    let mut at = first;
    while at <= state.t_left + state.per_px * f64::from(plot_width) {
        let x = state.x_of(at as u64, plot_left);
        if x >= plot_left {
            painter.line_segment(
                [Pos2::new(x, rect.bottom() - 5.0), Pos2::new(x, rect.bottom())],
                Stroke::new(0.5, theme.muted),
            );
            painter.text(
                Pos2::new(x + 2.0, rect.top() + 2.0),
                Align2::LEFT_TOP,
                label_time(state, at as u64),
                FontId::monospace(9.0),
                theme.muted,
            );
        }
        at += step;
    }

    // The stretch being dragged out, drawn on the axis as well: what a reader
    // wants to know mid-drag is which numbers the band lands between, and the
    // numbers are up here.
    if let Some(band) = state.band {
        let at = |time: f64| plot_left + ((time - state.t_left) / state.per_px) as f32;
        let (x0, x1) = (at(band.from.min(band.to)), at(band.from.max(band.to)));
        let (x0, x1) = (x0.max(plot_left), x1.min(rect.right()));
        if x1 > x0 {
            let over = Rect::from_min_max(Pos2::new(x0, rect.top()), Pos2::new(x1, rect.bottom()));
            painter.rect_filled(over, 0.0, theme.accent.gamma_multiply(0.14));
        }
    }

    // What each neighbouring pair of markers is apart, written between them.
    //
    // Pairs rather than every combination: three markers make three distances
    // and only two of them are being asked about, and a ruler with three
    // numbers overlapping on it answers nothing. The ends are the reader's to
    // add up, which is one subtraction they can do and this cannot guess.
    for pair in state.markers.windows(2) {
        let (x0, x1) = (state.x_of(pair[0], plot_left), state.x_of(pair[1], plot_left));
        if x1 < plot_left || x0 > rect.right() {
            continue;
        }
        let said = state.span_words(pair[1] - pair[0]);
        // Roughly 0.6em a glyph at this size, as `draw_bus` reckons it. Written
        // only where it fits between the two flags: half a number sitting over
        // a third flag is worse than no number.
        let wide = said.chars().count() as f32 * 4.8;
        if x1 - x0 < wide + 12.0 {
            continue;
        }
        // Along the bottom of the axis, under the numbers rather than through
        // them: measured, the two collided at 9pt in a 22px strip. The patch
        // behind is the ruler's own ground, so where a tick label would run
        // into the measurement the measurement wins — it is the more specific
        // answer, and the numbers either side of it still say where it is.
        let mid = (x0.max(plot_left) + x1.min(rect.right())) / 2.0;
        let y = rect.bottom() - 5.0;
        painter.rect_filled(
            Rect::from_center_size(Pos2::new(mid, y), Vec2::new(wide + 6.0, 10.0)),
            0.0,
            theme.surface_alt,
        );
        painter.text(
            Pos2::new(mid, y),
            Align2::CENTER_CENTER,
            &said,
            FontId::monospace(8.0),
            theme.muted,
        );
        // Rules out to the flags, so the number is read as belonging to the gap
        // rather than to the tick it happens to sit above.
        for (from, to) in [(x0, mid - wide / 2.0 - 3.0), (mid + wide / 2.0 + 3.0, x1)] {
            if to > from {
                painter.line_segment(
                    [Pos2::new(from.max(plot_left), y), Pos2::new(to.min(rect.right()), y)],
                    Stroke::new(0.5, theme.muted.gamma_multiply(0.6)),
                );
            }
        }
    }

    // The marked moments, as flags that can be picked up and put down again.
    //
    // A marker is nearly always dropped in about the right place and then
    // wanted exactly on an edge, and taking it down to put it back a pixel
    // along is two gestures for a correction. So a flag drags.
    let mut drop: Option<u64> = None;
    let mut moved: Option<(usize, u64)> = None;
    for (index, marker) in state.markers.clone().into_iter().enumerate() {
        let x = state.x_of(marker, plot_left);
        if x < plot_left || x > rect.right() {
            continue;
        }
        let flag = Rect::from_min_size(
            Pos2::new(x - FLAG_SLOP, rect.top()),
            Vec2::new(FLAG_SLOP * 2.0, rect.height()),
        );
        // An id of its own rather than one egui derives from the rectangle: a
        // flag being dragged moves, and an id that moved with it would end the
        // drag on the first frame the hand travelled.
        let response =
            ui.interact(flag, ui.id().with(("wave-marker", index)), Sense::click_and_drag());
        let colour =
            if response.hovered() || response.dragged() { theme.err } else { theme.accent };
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.5, colour),
        );
        painter.circle_filled(Pos2::new(x, rect.top() + 4.0), 3.0, colour);

        if response.dragged()
            && let Some(pos) = response.interact_pointer_pos()
        {
            moved = Some((index, state.time_at(pos.x.max(plot_left), plot_left)));
        }
        let response = response
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
            .on_hover_text("Drag to move this marker; click to take it down");
        if response.clicked() {
            drop = Some(marker);
        }
    }
    if let Some((index, to)) = moved {
        state.move_marker(index, to);
    }
    if let Some(marker) = drop {
        state.toggle_marker(marker);
    }

    // And the cursor, so the axis says where it is without arithmetic.
    if let Some(cursor) = state.cursor {
        let x = state.x_of(cursor, plot_left);
        if x >= plot_left && x <= rect.right() {
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(1.0, theme.cursor),
            );
            painter.text(
                Pos2::new(x + 3.0, rect.bottom() - 2.0),
                Align2::LEFT_BOTTOM,
                label_time(state, cursor),
                FontId::monospace(9.0),
                theme.cursor,
            );
        }
    }
}

/// A moment, in whatever unit the dump can vouch for.
fn label_time(state: &WaveState, at: u64) -> String {
    match state.dump.ns_of_ticks(at) {
        Some(0.0) => "0".to_string(),
        Some(ns) if ns >= 100.0 => format!("{ns:.0}"),
        Some(ns) if ns >= 1.0 => format!("{ns:.1}"),
        Some(ns) => format!("{ns:.3}"),
        None => at.to_string(),
    }
}

/// A round number of ticks near `raw`: one, two or five times a power of ten.
fn nice_step(raw: f64) -> f64 {
    if !raw.is_finite() || raw <= 0.0 {
        return 1.0;
    }
    let magnitude = 10f64.powf(raw.log10().floor());
    let scaled = raw / magnitude;
    let step = if scaled <= 1.0 {
        1.0
    } else if scaled <= 2.0 {
        2.0
    } else if scaled <= 5.0 {
        5.0
    } else {
        10.0
    };
    step * magnitude
}

/// A searchable list of what the dump and the design agree on.
fn signal_picker(ui: &mut Ui, state: &mut WaveState) {
    let mut add_all = false;
    ui.horizontal(|ui| {
        ui.label("find");
        ui.text_edit_singleline(&mut state.filter);
        // What the filter is for, most of the time: `m_byte` is eight signals
        // and clicking eight of them one at a time is the same work done
        // eight times.
        if let Some((_, tree)) = &state.picker {
            let total = tree.total();
            if total > 1 {
                add_all = ui
                    .button(format!("add all {total}"))
                    .on_hover_text("Everything the search matched, at once")
                    .clicked();
            }
        }
    });

    // Rebuilt only when the filter changes: a real dump has thousands of
    // variables, and splitting every path on every frame would be felt in the
    // one panel that has to stay responsive while being scrolled.
    let needle = state.filter.to_ascii_lowercase();
    if state.picker.as_ref().is_none_or(|(was, _)| *was != needle) {
        let matches = &state.matches;
        let tree =
            Scope::of(
                state.dump.vars().map(|(path, _)| path).filter(|path| {
                    needle.is_empty() || path.to_ascii_lowercase().contains(&needle)
                }),
                |path| matches.by_dump_path(path).is_some(),
            );
        state.picker = Some((needle.clone(), tree));
    }
    let Some((_, tree)) = &state.picker else { return };

    let mut add: Option<String> = None;
    let mut bulk: Vec<String> = Vec::new();
    if add_all {
        tree.paths(&mut bulk);
    }
    egui::ScrollArea::vertical().id_salt("wave-picker").max_height(220.0).show(ui, |ui| {
        if tree.is_empty() {
            ui.label(egui::RichText::new("nothing matches").weak());
            return;
        }
        // Open when something was asked for: a reader who typed a name wants
        // to see what it found, not a row of shut folders to open one by one.
        draw_scope(ui, tree, !needle.is_empty(), &mut add, &mut bulk);
    });

    if let Some(path) = add {
        state.status = match state.add_by_dump_path(&path) {
            Added::New => format!("added {path}"),
            Added::Already => format!("{path} is already on the panel"),
            Added::Missing => format!("`{path}` could not be read from the dump"),
        };
    }
    if !bulk.is_empty() {
        state.status = state.add_all(&bulk);
    }
}

/// How the name column is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Names {
    /// One line a track, each carrying the whole dotted path the dump spells.
    Path,
    /// The design's hierarchy, as a hierarchy: a line for each scope and the
    /// signals in it indented underneath, wearing the name they were given
    /// there rather than the whole path down to them.
    #[default]
    Tree,
}

/// One line of the name column.
///
/// The panel used to be a list of tracks and nothing else, and every row's
/// position *was* its index. A recording of a real design is
/// `tb_top.u_dut.u_ctrl.busy` twenty times over, and reading a hierarchy out
/// of twenty dotted strings is work the eye should not be doing when the
/// hierarchy is right there in the dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A scope, above the rows that live in it.
    Scope {
        /// The one component this line names, not the path down to it: the
        /// lines above it already said the rest.
        label: String,
        /// The whole path down to it, which is what a fold is remembered by.
        /// The label is not enough: two modules can both hold a `u_ctrl`.
        path: String,
        depth: usize,
        /// Every track under it, however deep — what clicking it picks, and
        /// what it goes on holding while it is folded shut.
        holds: Vec<usize>,
        /// Folded shut, so nothing under it is drawn.
        shut: bool,
    },
    /// A track, at its scope's depth plus one.
    Track { index: usize, depth: usize },
}

/// The name column, worked out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    pub rows: Vec<Row>,
    /// Which line each track is drawn on, and `None` for one inside a scope
    /// that is folded shut. The panel is addressed by line — a click is a
    /// `y`, and a `y` is a line — and this is the way back.
    ///
    /// A folded track is still *on the panel*: it keeps its place in the list,
    /// its comparison and its selection. Only the drawing leaves it out.
    pub line_of: Vec<Option<usize>>,
}

impl Layout {
    /// The track on this line, if it holds one.
    pub fn track_at(&self, line: usize) -> Option<usize> {
        match self.rows.get(line) {
            Some(Row::Track { index, .. }) => Some(*index),
            _ => None,
        }
    }

    pub fn lines(&self) -> usize {
        self.rows.len()
    }

    /// The position in the track list that a drop at this line boundary means.
    ///
    /// Boundaries run `0..=lines`: `0` is above the first line and `lines` is
    /// below the last. The answer is the track the dropped rows would sit in
    /// front of, which is what [`WaveState::move_rows`] takes, and the length
    /// of the list for a drop past the end.
    ///
    /// A scope's header line belongs to what is under it, so a drop on the gap
    /// above `u_rx` lands in front of `u_rx`'s first signal rather than after
    /// the module before it — the two are the same place in the list, and the
    /// first is the one a reader aiming at a module means.
    pub fn insertion(&self, boundary: usize) -> usize {
        self.rows
            .iter()
            .skip(boundary)
            .find_map(|row| match row {
                Row::Track { index, .. } => Some(*index),
                // A folded scope draws one line and holds many rows, and the
                // ones it holds are still in the list. Its first is where a
                // drop above it goes — reading past it would put the rows on
                // the far side of signals that are not on screen to be aimed
                // past.
                Row::Scope { holds, .. } => holds.first().copied(),
            })
            // Nothing below it holds a track: the drop was past the last row,
            // which means the end of the list.
            .unwrap_or(self.line_of.len())
    }
}

/// How far one level of hierarchy moves a name to the right.
const INDENT: f32 = 11.0;

/// How much of a scope's line the fold triangle takes, from its indent.
const FOLD_WIDTH: f32 = 14.0;

/// The triangle at the head of a scope, drawn rather than written.
///
/// A shape and not a glyph: every arrow and box this window reached for out of
/// a font came out hollow, five times, and the fix each time was to write a
/// word instead. There is no word for this one — it has to be the thing that
/// turns — so it is three points and a fill, which no font can fail to have.
fn fold_arrow(painter: &egui::Painter, left: f32, mid: f32, shut: bool, colour: Color32) {
    let (w, h) = (4.0, 4.5);
    let x = left + 4.0;
    let points = match shut {
        // Pointing right: there is something here that is not being shown.
        true => vec![Pos2::new(x, mid - h), Pos2::new(x + w * 1.6, mid), Pos2::new(x, mid + h)],
        // Pointing down, into what it opened.
        false => vec![
            Pos2::new(x - 0.5, mid - w * 0.8),
            Pos2::new(x + h * 1.8, mid - w * 0.8),
            Pos2::new(x + h * 0.65, mid + w * 1.2),
        ],
    };
    painter.add(egui::Shape::convex_polygon(points, colour, Stroke::NONE));
}

/// How many signals one gesture will put on the panel./// How many signals one gesture will put on the panel.
///
/// A bus is a few dozen and a scope can be thousands. Sixty-four is more than
/// any bus and fewer than any testbench, and asking for more than this is
/// answered rather than obeyed.
pub const ADD_AT_ONCE: usize = 64;

/// How many variables one scope will draw before it stops and says so.
///
/// A generated testbench can put ten thousand in one place. Drawing them all
/// costs a frame nobody asked for, and a list that long is not read anyway —
/// it is filtered.
const SCOPE_LIMIT: usize = 400;

/// One level of the dump's hierarchy.
///
/// Built out of the dotted paths rather than from the file's own scopes, which
/// wellen keeps to itself. The paths carry the same information: a dump writes
/// `tb.dut.u_rx.data` because that is where the variable sits.
#[derive(Default)]
struct Scope {
    name: String,
    children: Vec<Scope>,
    vars: Vec<Leaf>,
}

/// A variable, and whether the design turned out to have it.
struct Leaf {
    name: String,
    path: String,
    /// False when the dump has it and the design does not, which is worth
    /// saying rather than hiding: it is usually the testbench's own, and a
    /// reader who cannot find `dut.u_rx.data` wants to know it is absent, not
    /// to be shown a shorter list with no explanation.
    known: bool,
}

impl Scope {
    /// Dotted paths, as a tree.
    ///
    /// Takes the paths rather than the dump so that the shape of the answer can
    /// be tested without a recording on disk: what is interesting here is the
    /// splitting and the ordering, neither of which needs a real file.
    fn of<'a>(paths: impl Iterator<Item = &'a str>, known: impl Fn(&str) -> bool) -> Scope {
        let mut root = Scope::default();
        let mut paths: Vec<&str> = paths.collect();
        paths.sort_unstable();

        for path in paths {
            let mut parts = path.split('.').peekable();
            let mut at = &mut root;
            while let Some(part) = parts.next() {
                if parts.peek().is_none() {
                    at.vars.push(Leaf {
                        name: part.to_string(),
                        path: path.to_string(),
                        known: known(path),
                    });
                    break;
                }
                // `position` then index, because the borrow from `iter_mut`
                // would otherwise outlive the search that found it.
                let found = at.children.iter().position(|scope| scope.name == part);
                let index = match found {
                    Some(index) => index,
                    None => {
                        at.children.push(Scope { name: part.to_string(), ..Scope::default() });
                        at.children.len() - 1
                    }
                };
                at = &mut at.children[index];
            }
        }
        root
    }

    fn is_empty(&self) -> bool {
        self.vars.is_empty() && self.children.is_empty()
    }

    /// Every path under here, in the order the tree draws them.
    fn paths(&self, into: &mut Vec<String>) {
        for child in &self.children {
            child.paths(into);
        }
        into.extend(self.vars.iter().map(|leaf| leaf.path.clone()));
    }

    /// Everything under here, for the count on a folder.
    fn total(&self) -> usize {
        self.vars.len() + self.children.iter().map(Scope::total).sum::<usize>()
    }
}

/// One level of the tree, and everything under it.
fn draw_scope(
    ui: &mut Ui,
    scope: &Scope,
    open: bool,
    add: &mut Option<String>,
    bulk: &mut Vec<String>,
) {
    let theme = Theme::of(ui);
    // A level with one child asked the reader nothing, so it opens itself. That
    // is what walks `tb` and `dut` out of the way and lands on the signals,
    // while a level with real siblings still waits to be told which one.
    let solo = scope.children.len() == 1;
    for child in &scope.children {
        let label = format!("{}  ({})", child.name, child.total());
        let header = egui::CollapsingHeader::new(label)
            .id_salt(&child.name)
            .default_open(open || solo)
            .show(ui, |ui| draw_scope(ui, child, open, add, bulk));
        // On the header rather than inside it, so a scope can be taken whole
        // without being opened first — which is the case where opening it is
        // pure ceremony, because the reader already knows what a module's
        // signals are.
        header.header_response.context_menu(|ui| {
            if ui.button(format!("add all {} here", child.total())).clicked() {
                child.paths(bulk);
                ui.close();
            }
        });
    }
    for leaf in scope.vars.iter().take(SCOPE_LIMIT) {
        let text = egui::RichText::new(&leaf.name).monospace();
        let text = match leaf.known {
            true => text,
            false => text.color(theme.muted),
        };
        let row = ui.selectable_label(false, text);
        let row = match leaf.known {
            true => row,
            false => row.on_hover_text("in the dump, but not in the design"),
        };
        if row.clicked() {
            *add = Some(leaf.path.clone());
        }
    }
    if scope.vars.len() > SCOPE_LIMIT {
        let rest = scope.vars.len() - SCOPE_LIMIT;
        ui.label(
            egui::RichText::new(format!("…and {rest} more here; narrow the search")).small().weak(),
        );
    }
}

/// How many signals a dump opens showing.
///
/// The module's own ports and no deeper. Enough that the panel answers "what
/// happened" the moment it opens, few enough that a design with four hundred
/// ports does not fill the screen with rows nobody asked for.
const TRACKS_AT_FIRST: usize = 12;

/// Whether a recorded name reads as a clock or a reset.
///
/// A guess, and only ever used to order rows in a recording opened with no
/// design — never to decide what a signal *is*. Whole words, so `clocks_done`
/// and `unlock` are left where they were: a substring test on `clk` promotes
/// half a testbench.
fn looks_like_a_clock(path: &str) -> bool {
    let leaf = path.rsplit('.').next().unwrap_or(path).to_ascii_lowercase();
    leaf.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| matches!(word, "clk" | "clock" | "rst" | "reset" | "resetn" | "rstn"))
}

/// How much of the name column the remove button takes.
const REMOVE_WIDTH: f32 = 22.0;

/// A small × beside each track, since a panel that only fills up is no use.
fn remove_buttons(ui: &mut Ui, state: &mut WaveState, rect: Rect) {
    let mut remove: Option<usize> = None;
    let layout = state.layout();
    for index in 0..state.tracks.len() {
        // A folded row has no line, so it has no × either.
        let Some(line) = layout.line_of.get(index).copied().flatten() else { continue };
        let top = rect.top() + TRACK_HEIGHT * line as f32 + 2.0;
        let at = Rect::from_min_size(
            Pos2::new(rect.left() + NAME_WIDTH - 18.0, top + 3.0),
            Vec2::splat(14.0),
        );
        let button = ui.put(at, egui::Button::new("×").small().frame(false));
        if button.clicked() {
            remove = Some(index);
        }
        // Said here because this is where somebody clearing a panel one row at
        // a time is looking, and it is the moment the faster way is worth
        // knowing.
        if index == 0 {
            button.on_hover_text(
                "Take this row off. Ctrl-click or shift-click names to pick several, \
                 then Delete.",
            );
        }
    }
    if let Some(index) = remove {
        state.remove(index);
    }
}

/// The right-click menu: put a marker down, or take one back up.
///
/// Markers are how this panel answers "how long between these two", and until
/// now the only way to drop one was the `M` key with the cursor already in the
/// right place. That is two gestures for one intention, and the second is
/// undiscoverable.
///
/// One item, whose words change with what is under the pointer, rather than two
/// that are alternately useless. `toggle_marker` decides which it is; this only
/// has to say so.
fn marker_menu(state: &mut WaveState, response: &egui::Response) {
    let Some(at) = state.menu_at else { return };
    // Within a few pixels, not the same tick: a reader clicking near a flag
    // means that flag, and no hand lands on one tick out of three hundred
    // thousand.
    let slop = (state.per_px * f64::from(FLAG_SLOP)) as u64;
    let near = state.markers.iter().find(|marker| marker.abs_diff(at) <= slop).copied();

    response.context_menu(|ui| {
        // Wide enough that the item reads as one line: a menu that wraps its
        // only sentence looks like something went wrong.
        ui.set_min_width(170.0);
        let label = match near {
            Some(_) => "Take this marker down",
            None => "Put a marker here",
        };
        if ui.button(label).clicked() {
            state.toggle_marker(near.unwrap_or(at));
            state.status = match near {
                Some(_) => "marker taken down".to_string(),
                None => format!("marked at {}", label_time(state, at)),
            };
            state.menu_at = None;
            ui.close();
        }
        // A second marker is what makes the first one useful, so the panel says
        // what it is measuring rather than leaving it to be noticed.
        if state.markers.len() >= 2 {
            ui.separator();
            let span = state.markers[state.markers.len() - 1] - state.markers[0];
            ui.label(
                egui::RichText::new(format!(
                    "{} across {} markers",
                    state.span_words(span),
                    state.markers.len()
                ))
                .weak(),
            );
        }
    });
}

/// Where the pointer is, if it is anywhere.
fn pointer(ui: &Ui) -> Option<egui::Pos2> {
    ui.input(|input| input.pointer.hover_pos())
}

/// Whether a notch of the wheel belongs to the list of rows rather than to the
/// scale, given where the panel starts.
///
/// The name column is a list and the plot is a picture of time, and the wheel
/// means something different over each. The boundary is the column's own
/// width, so it is the line the reader can see — the names end and the
/// waveforms begin, and so does the other gesture.
///
/// With no pointer at all, neither: nothing is hovered, so nothing moves.
fn wheel_moves_the_list(at: Option<egui::Pos2>, panel_left: f32) -> bool {
    at.is_some_and(|at| at.x < panel_left + NAME_WIDTH)
}

/// Pan, zoom and the cursor.
fn handle_input(
    ui: &Ui,
    state: &mut WaveState,
    response: &egui::Response,
    plot_left: f32,
    plot_width: f32,
) {
    // Zoom about the pointer, so the thing under it stays under it — and only
    // where the pointer is on the plot. Over the names the wheel belongs to the
    // rows; see the scroll area that holds them.
    let at = pointer(ui);
    if response.hovered() && !wheel_moves_the_list(at, plot_left - NAME_WIDTH) {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        let zoom = ui.input(|i| i.zoom_delta());
        if zoom != 1.0 || scroll != 0.0 {
            let factor = if zoom != 1.0 {
                1.0 / f64::from(zoom)
            } else {
                (1.0 - f64::from(scroll) * 0.002).clamp(0.5, 2.0)
            };
            let anchor = at.map_or(plot_left + plot_width / 2.0, |p| p.x);
            let at = state.time_at(anchor, plot_left) as f64;
            state.per_px = (state.per_px * factor).max(f64::MIN_POSITIVE);
            state.t_left = at - f64::from(anchor - plot_left) * state.per_px;
        }
    }

    // A drag across the plot sweeps out a stretch of time; the same drag with
    // shift held pans, as it always did. Panning is the gesture that survives
    // in the name column too, so a reader who wants to slide the view has two
    // ways to and neither of them is hidden.
    //
    // The wheel still zooms about the pointer, which is the fast way; the band
    // is the exact way — "show me from here to here" is a question a reader can
    // point at but not compute.
    let shift = ui.input(|input| input.modifiers.shift);

    // Armed from where the press began rather than on the frame egui decides a
    // drag has started. Measured: that frame goes past without the pointer
    // position this needs — the band never armed, and the drag fell through to
    // the pan branch, which is the behaviour it was replacing. `press_origin`
    // is stable for the whole press, so there is no frame to miss.
    if response.dragged() && !shift {
        let origin = ui.input(|input| input.pointer.press_origin());
        if let Some(origin) = origin
            && origin.x >= plot_left
        {
            let now = ui.input(|input| input.pointer.latest_pos()).map_or(origin.x, |pos| pos.x);
            let (from, to) =
                (state.time_at_raw(origin.x, plot_left), state.time_at_raw(now, plot_left));
            match state.band.as_mut() {
                Some(band) => band.to = to,
                None => state.band = Some(Band { from, to }),
            }
        }
    }

    // A drag down the names moves rows. Dragging a row is how every list
    // anybody has used is reordered, and the order of this one is an argument
    // the reader is making — a clock, then the valid, then the data it
    // qualifies — which no analysis in this program can make for them.
    //
    // The pan this gesture used to do in the name column is on shift now, which
    // is where the plot already keeps it, and the scrollbar under the tracks
    // does it without a modifier at all. Neither is hidden.
    if response.dragged() && !shift && state.band.is_none() {
        let origin = ui.input(|input| input.pointer.press_origin());
        if let Some(origin) = origin
            && origin.x < plot_left - REMOVE_WIDTH
        {
            let layout = state.layout();
            if state.moving.is_none() {
                // Taken hold of where the press began rather than where the
                // pointer is on the frame egui calls it a drag. By then the
                // hand has moved, and the row it started on is the row it meant
                // — the same reason the band arms from `press_origin`.
                let line = ((origin.y - response.rect.top()) / TRACK_HEIGHT) as usize;
                let rows = grabbed(state, &layout, line);
                if !rows.is_empty() {
                    // What is moving has to be what is lit, or a reader cannot
                    // see the block they are carrying. A grab inside the
                    // selection leaves it alone, anchor and all: those rows
                    // were already picked, and re-picking them would throw away
                    // which one the reader started from.
                    if !rows.iter().all(|row| state.selection.contains(row)) {
                        state.selection = rows.clone();
                    }
                    state.moving = Some(Moving { rows, at: line });
                }
            }
            if let Some(moving) = state.moving.as_mut() {
                let y = ui.input(|input| input.pointer.latest_pos()).map_or(origin.y, |pos| pos.y);
                // Half a row down, so the boundary the drop means is the
                // nearest gap rather than the top of whatever row the pointer
                // happens to be inside.
                let boundary = ((y - response.rect.top()) / TRACK_HEIGHT + 0.5).max(0.0) as usize;
                moving.at = boundary.min(layout.lines());
            }
        }
    }

    // No band and nothing being carried: the drag pans, as it always did.
    if response.dragged() && state.band.is_none() && state.moving.is_none() {
        state.t_left -= f64::from(response.drag_delta().x) * state.per_px;
    }

    // A right-click asks about the moment under it, so the moment is taken
    // now. `context_menu` below runs while the menu is open, by which time the
    // pointer is over the menu and not over the waveform at all.
    if response.secondary_clicked()
        && let Some(pos) = response.interact_pointer_pos()
        && pos.x >= plot_left
    {
        state.menu_at = Some(state.time_at(pos.x, plot_left));
    }

    // Escape abandons a band. A gesture with no way out is one people stop
    // starting.
    //
    // Before the release rather than after it, because egui cancels a drag when
    // Escape is pressed and reports a cancelled drag as a stopped one. Measured:
    // Escape mid-sweep zoomed to the band it was meant to throw away, which is
    // the one thing the key exists not to do.
    if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
        state.band = None;
        // And puts down whatever was being carried, unmoved. Every gesture
        // here has to have a way out, or people stop starting them.
        state.moving = None;
    }

    // The release is tested before the band is taken: a let-chain evaluates
    // left to right, and taking it first threw the band away on every frame of
    // the drag.
    if response.drag_stopped()
        && let Some(band) = state.band.take()
    {
        let per_px = state.per_px;
        let at = |time: f64| plot_left + ((time - state.t_left) / per_px) as f32;
        match dragged(at(band.from), at(band.to)) {
            Dragged::Zoom => state.zoom_to(band.from, band.to, plot_width),
            // Too short to be a sweep: it was a press, and a press sets the
            // cursor. Done here rather than left to `clicked()`, because egui
            // calls a press that moved at all a drag and not a click.
            Dragged::Click => state.cursor = Some(band.from.max(0.0) as u64),
        }
    }

    // The rows are put down where the line said they would go. Said out loud,
    // because a move that landed one row from where it was aimed looks exactly
    // like a move that did nothing — and a drop that changed nothing says
    // nothing, rather than claiming a move it did not make.
    if response.drag_stopped()
        && let Some(moving) = state.moving.take()
    {
        let before = state.layout().insertion(moving.at);
        let carried = moving.rows.len();
        if state.move_rows(&moving.rows, before) {
            state.status = match carried {
                1 => "moved 1 row".to_string(),
                many => format!("moved {many} rows"),
            };
        }
    }

    state.hold_in_view(plot_width);

    if response.clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        if pos.x > plot_left {
            state.cursor = Some(state.time_at(pos.x, plot_left));
        } else if pos.x < plot_left - REMOVE_WIDTH {
            // The names are the handle: clicking one says "this row", which is
            // what the keyboard then acts on. The strip the × sits in is left
            // alone, or selecting would fight with removing.
            let line = ((pos.y - response.rect.top()) / TRACK_HEIGHT) as usize;
            let (toggle, reach) = held_for_the_click(ui);
            match state.layout().rows.get(line).cloned() {
                Some(Row::Track { index, .. }) => state.click_row(index, toggle, reach),
                Some(Row::Scope { path, depth, holds, .. }) => {
                    // The triangle turns it; the rest of the line picks what
                    // is under it. Two meanings on one row, told apart the way
                    // every tree tells them apart — by where the hand landed.
                    let arrow = response.rect.left() + 4.0 + INDENT * depth as f32;
                    match pos.x < arrow + FOLD_WIDTH {
                        true => {
                            state.fold(&path);
                            // Said out loud, because folding takes rows off
                            // the screen and taking them off the panel is the
                            // other thing this window does to rows. A reader
                            // who mixed the two up would go looking for
                            // signals they think they deleted.
                            state.status = match state.is_shut(&path) {
                                true => format!(
                                    "`{path}` folded — its {} row(s) are still on the panel",
                                    holds.len()
                                ),
                                false => format!("`{path}` opened"),
                            };
                        }
                        false => state.pick_all(&holds, toggle),
                    }
                }
                // Below the last line is the panel's own empty space, and a
                // click there means "none of them".
                None => state.select_none(),
            }
        }
    }

    keys(ui, state, response, plot_width);
}

/// What a press on this line takes hold of, as positions in the track list.
///
/// A row that is already picked carries the whole selection with it, which is
/// what makes moving five scattered signals one gesture rather than five. A row
/// that is not picked carries itself alone — the reader is pointing at it, not
/// at what they picked a minute ago. A scope carries everything under it,
/// folded or not, because a module is the thing on that line.
fn grabbed(state: &WaveState, layout: &Layout, line: usize) -> Vec<usize> {
    match layout.rows.get(line) {
        Some(Row::Track { index, .. }) => match state.selection.contains(index) {
            true => state.selected_rows(),
            false => vec![*index],
        },
        Some(Row::Scope { holds, .. }) => holds.clone(),
        // Below the last row: the hand is on the panel's own empty space, and
        // there is nothing there to pick up.
        None => Vec::new(),
    }
}

/// What was held down for the click being handled: `ctrl`, then `shift`.
///
/// Taken from the click's own event rather than from what is held *now*.
/// `clicked()` is answered on the frame the button came up, and by then a
/// modifier can already have been let go — a click and its keys are one
/// gesture to the hand and two streams of events to the window. Measured with
/// a synthetic ctrl-click whose Ctrl was released 80 ms after the button: the
/// row was selected as though nothing had been held, which is the same thing a
/// fast hand would do.
fn held_for_the_click(ui: &Ui) -> (bool, bool) {
    ui.input(|input| {
        input
            .events
            .iter()
            .rev()
            .find_map(|event| match event {
                egui::Event::PointerButton {
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers,
                    ..
                } => Some((modifiers.command, modifiers.shift)),
                _ => None,
            })
            // Nothing in this frame's events says otherwise, so what is held
            // now is the best answer there is.
            .unwrap_or((input.modifiers.command, input.modifiers.shift))
    })
}

/// The wire the selection has just moved to, once.
///
/// Read here rather than at each place that moves it, because the selection
/// moves from a click on a name, from the arrow keys, and from a track being
/// removed out from under it. One place that notices covers all three, and
/// cannot be forgotten by the next one added.
fn newly_selected(state: &mut WaveState) -> Option<SignalId> {
    if state.selected() == state.announced {
        return None;
    }
    state.announced = state.selected();
    match state.tracks.get(state.selected()?) {
        Some(Track::Signal { signal, .. }) => *signal,
        _ => None,
    }
}

/// The keyboard, which only acts while the panel is under the pointer.
///
/// A window with a text field in it must not have its arrow keys stolen, so
/// nothing here fires while anything has focus — the find box is right there in
/// the same toolbar.
fn keys(ui: &Ui, state: &mut WaveState, response: &egui::Response, plot_width: f32) {
    if !response.hovered() || ui.memory(|memory| memory.focused().is_some()) {
        return;
    }
    let (back, forward, mark, delete, up, down, alt) = ui.input(|input| {
        (
            input.key_pressed(egui::Key::ArrowLeft),
            input.key_pressed(egui::Key::ArrowRight),
            input.key_pressed(egui::Key::M),
            input.key_pressed(egui::Key::Delete) || input.key_pressed(egui::Key::Backspace),
            input.key_pressed(egui::Key::ArrowUp),
            input.key_pressed(egui::Key::ArrowDown),
            input.modifiers.alt,
        )
    });

    // Alt and an arrow moves the picked rows, the way it moves a line in an
    // editor. Alt, because the bare arrows belong to the cursor here — ← and →
    // walk a signal's edges — and a panel whose ↑ and ↓ meant something in a
    // different world from its ← and → is two keyboards on one list.
    //
    // The drag is the gesture; this is for a hand already on the keys, and for
    // a list longer than the window, where the far end of a drag is somewhere
    // the pointer cannot reach without scrolling under it.
    if alt && (up || down) {
        let carried = state.selected_rows().len();
        state.status = if carried == 0 {
            "click a name first — the arrows move the rows you picked".to_string()
        } else if state.move_selected(down) {
            match carried {
                1 => "moved 1 row".to_string(),
                many => format!("moved {many} rows"),
            }
        } else {
            match down {
                true => "already at the bottom".to_string(),
                false => "already at the top".to_string(),
            }
        };
    }

    // What the × does one at a time. The × stays: a single row is one click
    // either way, and a button is the only one of the two that can be found
    // without being told about it.
    if delete && !state.selection.is_empty() {
        let taken = state.remove_selected();
        state.status = match taken {
            1 => "took 1 row off the panel".to_string(),
            many => format!("took {many} rows off the panel"),
        };
    }

    if mark && let Some(cursor) = state.cursor {
        state.toggle_marker(cursor);
        state.status = match state.markers().contains(&cursor) {
            true => "marked".to_string(),
            false => "marker taken down".to_string(),
        };
    }

    if !(back || forward) {
        return;
    }
    let Some(track) = state.selected() else {
        state.status = "click a signal's name first — an edge belongs to one signal".to_string();
        return;
    };
    let from = state.cursor.unwrap_or(0);
    match state.edge(track, from, forward) {
        Some(at) => state.jump_to(at, plot_width),
        None => {
            state.status = match forward {
                true => "nothing changes after here".to_string(),
                false => "nothing changes before here".to_string(),
            }
        }
    }
}

fn draw(ui: &Ui, state: &mut WaveState, rect: Rect, plot_left: f32, plot_width: f32) {
    let painter = ui.painter_at(rect);
    let theme = Theme::of(ui);
    let (text, faint, rule) = (theme.ink, theme.muted, theme.line);
    let (line, busy, bad) = (theme.wave_line, theme.wave_busy, theme.err);
    let dark = ui.visuals().dark_mode;

    // The names sit on their own ground, so the eye reads two columns rather
    // than one field of text that happens to have waveforms in the right of
    // it. Same fill as the ruler, which is the panel's other strip of
    // furniture.
    painter.rect_filled(
        Rect::from_min_max(rect.left_top(), Pos2::new(plot_left, rect.bottom())),
        0.0,
        theme.surface_alt,
    );

    // The ruler's own marks, carried down through the tracks. Under everything
    // else, and dotted rather than drawn, because this is the one line on the
    // panel that means nothing by itself: it is there so an eye can carry a
    // moment from one row to another without a straight edge.
    grid(&painter, state, rect, plot_left, plot_width, rule);

    // Worked out once a frame rather than kept and invalidated: where a row
    // goes depends on every other row, so every add and every remove would have
    // to remember to rebuild it, and the one that forgot would draw the column
    // against rows that are no longer there. It is a walk over names that are
    // about to be laid out anyway.
    let layout = state.layout();
    let shown = state.shown_names();

    // The scopes first, under everything: they are the ground the signals in
    // them are drawn on, not a row of their own kind.
    for (at, row) in layout.rows.iter().enumerate() {
        let Row::Scope { label, depth, holds, shut, .. } = row else { continue };
        let top = rect.top() + TRACK_HEIGHT * at as f32 + 2.0;
        let mid = top + TRACK_HEIGHT / 2.0;
        // Lit when everything under it is picked, which is what clicking it
        // does — so the line says whether that already happened.
        let all = !holds.is_empty() && holds.iter().all(|index| state.selection.contains(index));
        if all {
            painter.rect_filled(
                Rect::from_min_size(
                    Pos2::new(rect.left(), top),
                    Vec2::new(plot_left - rect.left(), TRACK_HEIGHT),
                ),
                0.0,
                theme.accent_soft,
            );
        }
        let left = rect.left() + 4.0 + INDENT * *depth as f32;
        fold_arrow(&painter, left, mid, *shut, faint);
        painter.text(
            Pos2::new(left + FOLD_WIDTH, mid),
            Align2::LEFT_CENTER,
            format!("{label}  ({})", holds.len()),
            FontId::monospace(11.0),
            faint,
        );
    }

    for index in 0..state.tracks.len() {
        // Not `line`: that is the colour a waveform is drawn in, three lets up.
        // Nothing at all when a fold has taken this row away — the track is
        // still on the panel, and only the drawing leaves it out.
        let Some(at) = layout.line_of.get(index).copied().flatten() else { continue };
        let depth = match layout.rows.get(at) {
            Some(Row::Track { depth, .. }) => *depth,
            _ => 0,
        };
        let top = rect.top() + TRACK_HEIGHT * at as f32 + 2.0;
        let mid = top + TRACK_HEIGHT / 2.0;
        if state.selection.contains(&index) {
            painter.rect_filled(
                Rect::from_min_size(
                    Pos2::new(rect.left(), top),
                    Vec2::new(rect.width(), TRACK_HEIGHT),
                ),
                0.0,
                theme.accent_soft,
            );
        }
        // The rule between the two columns, per row rather than one long line,
        // so the selected row's own colour is not cut in half by it.
        painter.line_segment(
            [Pos2::new(plot_left, top), Pos2::new(plot_left, top + TRACK_HEIGHT)],
            Stroke::new(0.5, rule),
        );
        painter.line_segment(
            [Pos2::new(rect.left(), top), Pos2::new(rect.right(), top)],
            Stroke::new(0.5, rule),
        );

        let (name, kind) = match &state.tracks[index] {
            Track::Signal { name, width, .. } => (name.clone(), Kind::Signal(*width)),
            Track::Decoded { decode, row, name } => (name.clone(), Kind::Decoded(*decode, *row)),
            Track::Stage { stage, name } => (name.clone(), Kind::Stage(*stage)),
        };
        // What is written is not always what the row *is*: `differs` and the
        // reference readout are about the recorded path, and shortening is a
        // fact about this panel rather than about the signal.
        let written = shown.get(index).cloned().unwrap_or_else(|| name.clone());

        // The name, and what it held at the cursor.
        let value_text = match kind {
            Kind::Signal(_) => state.value_of(Layer::Recorded, index).unwrap_or_default(),
            _ => String::new(),
        };
        // Cut to what the column holds, from the *left*. A dump's names are
        // one long prefix and a short distinguishing tail —
        // `tb_pipeline_demo.u_dut.core_clk` — so trimming the end throws away
        // the only part that tells one row from the next. Before this the name
        // simply ran on, under the remove button and out over the waveform.
        let font = FontId::monospace(11.0);
        // What the readout will take, kept back before the name is measured.
        // The two share this column, and the number is the one that has to be
        // read exactly.
        let taken = match value_text.is_empty() {
            true => 0.0,
            false => {
                painter
                    .layout_no_wrap(value_text.clone(), font.clone(), Color32::PLACEHOLDER)
                    .size()
                    .x
                    + 8.0
            }
        };
        let left = rect.left() + 4.0 + INDENT * depth as f32;
        let room = plot_left - REMOVE_WIDTH - 8.0 - taken - left;
        let drawn = painter.text(
            Pos2::new(left, mid),
            Align2::LEFT_CENTER,
            trimmed(&painter, &written, &font, room),
            font,
            // The anchor alone, not every picked row: it is the one the
            // keyboard and the buttons under the panel are about, and a column
            // of accent-coloured names would say they all were.
            if state.selected() == Some(index) { theme.accent } else { text },
        );
        // `!=` rather than `≠`, for the same reason as `go` and not an arrow:
        // the monospace face here has no glyph for it and draws a box.
        if state.reference.as_ref().is_some_and(|it| it.differs(&name)) {
            painter.text(
                Pos2::new(drawn.right() + 5.0, mid),
                Align2::LEFT_CENTER,
                "!=",
                FontId::monospace(11.0),
                theme.err,
            );
        }
        if !value_text.is_empty() {
            // Red when the two recordings held different things *here*. A bus
            // is drawn as a band whichever value it carries, so the ghost
            // behind it says nothing; the number is where the difference is,
            // and the moment is the reader's actual question.
            let parts = state.value_of(Layer::Reference, index).is_some_and(|it| it != value_text);
            painter.text(
                // Clear of the remove button rather than up against the plot:
                // a four-character hex value fitted in the gap, and a state
                // name does not — it came out with an x through its last
                // letter.
                Pos2::new(plot_left - REMOVE_WIDTH - 4.0, mid),
                Align2::RIGHT_CENTER,
                &value_text,
                FontId::monospace(11.0),
                if parts { theme.err } else { faint },
            );
        }

        match kind {
            Kind::Decoded(decode, row) => draw_annotations(
                state, &painter, theme, decode, row, top, plot_left, plot_width, text,
            ),
            Kind::Signal(width) => {
                // The reference first and faint, so it shows through only
                // where the two part — which is the whole question being asked.
                let names = state.names_for(index);
                if state.reference.is_some() {
                    let ghost = state.runs_for(Layer::Reference, index, plot_width);
                    let dim = |colour: Color32| colour.gamma_multiply(GHOST);
                    match width > 1 {
                        true => draw_bus(
                            &painter,
                            &ghost,
                            top,
                            plot_left,
                            dim(theme.box_stroke),
                            dim(text),
                            dim(busy),
                            dim(bad),
                            dark,
                            names,
                        ),
                        false => draw_bit(
                            &painter,
                            &ghost,
                            top,
                            plot_left,
                            dim(line),
                            dim(busy),
                            dim(bad),
                        ),
                    }
                }
                let runs = state.runs_for(Layer::Recorded, index, plot_width);
                match width > 1 {
                    true => {
                        // Outlined in ink rather than in the trace colour: the
                        // fill is what identifies the value, and a coloured
                        // outline around a coloured fill leaves the panel one
                        // hue with shapes in it.
                        draw_bus(
                            &painter,
                            &runs,
                            top,
                            plot_left,
                            theme.box_stroke,
                            theme.ink,
                            busy,
                            bad,
                            dark,
                            names,
                        )
                    }
                    false => draw_bit(&painter, &runs, top, plot_left, line, busy, bad),
                }
            }
            Kind::Stage(stage) => {
                draw_stage(state, &painter, theme, stage, top, plot_left, plot_width, text)
            }
        }
    }

    // The marked moments, drawn down the tracks so a span can be seen rather
    // than only read off the toolbar.
    for marker in &state.markers {
        let x = state.x_of(*marker, plot_left);
        if x >= plot_left && x <= rect.right() {
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(1.0, theme.accent.gamma_multiply(0.7)),
            );
        }
    }

    // The stretch being dragged out, under the cursor and over the traces: it
    // is a question being asked, not something the recording says.
    if let Some(band) = state.band {
        let at = |time: f64| plot_left + ((time - state.t_left) / state.per_px) as f32;
        let (x0, x1) = (at(band.from.min(band.to)), at(band.from.max(band.to)));
        let (x0, x1) = (x0.max(plot_left), x1.min(rect.right()));
        if x1 > x0 {
            let over = Rect::from_min_max(Pos2::new(x0, rect.top()), Pos2::new(x1, rect.bottom()));
            painter.rect_filled(over, 0.0, theme.accent.gamma_multiply(0.18));
            for edge in [x0, x1] {
                painter.line_segment(
                    [Pos2::new(edge, rect.top()), Pos2::new(edge, rect.bottom())],
                    Stroke::new(1.0, theme.accent),
                );
            }
        }
    }

    // The cursor, over everything.
    if let Some(at) = state.cursor {
        let x = state.x_of(at, plot_left);
        if x >= plot_left && x <= rect.right() {
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(1.0, theme.cursor),
            );
        }
    }

    // What the pointer is over on a decoder lane, in full.
    if let Some(pointer) = ui.input(|input| input.pointer.hover_pos())
        && pointer.x > plot_left
        && rect.contains(pointer)
    {
        let line = ((pointer.y - rect.top()) / TRACK_HEIGHT) as usize;
        let index = state.layout().track_at(line).unwrap_or(usize::MAX);
        if let Some(Track::Decoded { decode, row, .. }) = state.tracks.get(index)
            && let Some(report) = state.decodes.get(*decode)
            && let Some(annotation) =
                annotation_at(report, *row, state.time_at(pointer.x, plot_left))
        {
            let mut lines = vec![annotation.label.clone()];
            lines.extend(annotation.fields.iter().map(|(k, v)| format!("{k}: {v}")));
            // Painted rather than handed to a tooltip widget: the lanes are
            // drawn, not laid out, so there is no response to hang one on.
            let text = lines.join(
                "
",
            );
            let font = FontId::monospace(10.0);
            let galley = painter.layout_no_wrap(text, font, text_colour(ui));
            let at = pointer + Vec2::new(12.0, 12.0);
            let box_rect = Rect::from_min_size(at, galley.size()).expand(4.0);
            painter.rect_filled(box_rect, 3.0, ui.visuals().panel_fill);
            painter.rect_stroke(
                box_rect,
                3.0,
                Stroke::new(1.0, ui.visuals().weak_text_color()),
                egui::StrokeKind::Inside,
            );
            painter.galley(at, galley, text_colour(ui));
        }
    }

    // Where the rows being carried would land, over everything else. A gap
    // between two rows is not a thing anybody can see, so the answer to "where
    // will this go" has to be drawn: without it a reorder is a guess, and a
    // guess that lands one row out looks exactly like a gesture that failed.
    //
    // Across the names alone, because that column is the list being reordered.
    // A line reaching over the traces would read as a moment, which is what
    // every other line on that half of the panel means.
    if let Some(moving) = &state.moving {
        let y = rect.top() + TRACK_HEIGHT * moving.at as f32 + 2.0;
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(plot_left, y)],
            Stroke::new(2.0, theme.accent),
        );
    }
}

/// What a row is, once its name has been taken out of it.
enum Kind {
    Signal(u32),
    Decoded(usize, u8),
    Stage(usize),
}

/// One stage of the pipeline, a box per cycle.
///
/// Boxes rather than bins: a cycle is a real unit here, and reducing several of
/// them into one column would produce a picture that no longer says which cycle
/// anything happened in. So past the window the row says to zoom in instead of
/// drawing a smear that reads like data.
#[allow(clippy::too_many_arguments)]
fn draw_stage(
    state: &mut WaveState,
    painter: &egui::Painter,
    theme: &Theme,
    stage: usize,
    top: f32,
    left: f32,
    plot_width: f32,
    text: Color32,
) {
    let (t_left, per_px) = (state.t_left, state.per_px);
    let t_right = t_left + per_px * f64::from(plot_width);
    let x_of = |time: u64| left + ((time as f64 - t_left) / per_px) as f32;

    let too_wide = {
        let Some(cycles) = state.cycles.as_ref() else { return };
        let last = cycles.len().saturating_sub(1);
        let from = cycles.cycle_at(t_left.max(0.0) as u64).unwrap_or(0);
        let to = cycles.cycle_at(t_right.max(0.0) as u64).unwrap_or(last);
        to.saturating_sub(from) > rtlscope_wave::stages::MAX_WINDOW
    };
    if too_wide {
        painter.text(
            Pos2::new(left + 6.0, top + TRACK_HEIGHT / 2.0),
            Align2::LEFT_CENTER,
            "zoom in to see cycles",
            FontId::monospace(10.0),
            text,
        );
        return;
    }

    let Some(view) = state.stages_in_view(t_right) else { return };
    let Some(row) = view.rows.iter().find(|row| row.stage == stage) else { return };
    // The last cell has no following edge to end at, so it borrows the length
    // of the first one.
    let period = view.times.windows(2).next().map_or(1, |pair| pair[1] - pair[0]);

    let box_top = top + 3.0;
    let box_bottom = top + TRACK_HEIGHT - 3.0;
    for (column, cell) in row.cells.iter().enumerate() {
        let colour = match cell {
            StageCell::Busy => theme.ok.gamma_multiply(0.45),
            StageCell::Held => theme.warn.gamma_multiply(0.55),
            StageCell::Unknown => theme.err.gamma_multiply(0.55),
            StageCell::Idle | StageCell::Blank => continue,
        };
        let Some(start) = view.times.get(column).copied() else { break };
        let end = view.times.get(column + 1).copied().unwrap_or(start + period);
        let (x0, x1) = (x_of(start).max(left), x_of(end).min(left + plot_width));
        if x1 < left || x0 > left + plot_width {
            continue;
        }
        let rect =
            Rect::from_min_max(Pos2::new(x0, box_top), Pos2::new(x1.max(x0 + 1.0), box_bottom));
        painter.rect_filled(rect, 1.0, colour);

        // The value only where the box is wide enough to hold it.
        if rect.width() > 26.0
            && let Some(value) = row.values.get(column)
            && !value.is_empty()
        {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                elide(value, (rect.width() / 6.0) as usize),
                FontId::monospace(9.0),
                text,
            );
        }
    }
}

fn text_colour(ui: &Ui) -> Color32 {
    Theme::of(ui).ink
}

fn shown(value: &WaveValue) -> String {
    match value.as_u64() {
        Some(number) if value.width() == 1 => number.to_string(),
        Some(number) => format!("0x{number:x}"),
        None => value.bit_string(),
    }
}

/// A bus, as runs of one value with the value written in each.
///
/// Every waveform viewer draws a bus this way and the reason is that the shape
/// carries nothing: a byte holding `0x1d` and a byte holding `0x00` are the
/// same rectangle. Without the number the row says only *when* something
/// changed, which is the half of the question a reader already had.
///
/// Runs shorter than the text are left empty rather than scribbled over. A
/// value that does not fit is one the reader can get by putting the cursor on
/// it, and a column of overlapping half-digits is worse than a clean gap.
/// A bus's fill, taken from the value it carries.
///
/// The same value is the same colour wherever it appears, which is the point:
/// a reader scanning a row for "when was it 0x1c again" gets an answer without
/// reading every label. Hue only — saturation and value are fixed per ground so
/// no value shouts louder than another, and the text stays legible over all of
/// them.
///
/// This is decoration in the sense that nothing depends on *which* hue a value
/// gets, and not decoration in the sense that the theme's rule is about: the
/// meaning colours stay reserved, because "these two stretches hold different
/// things" is not a verdict about either of them.
fn bus_fill(value: &WaveValue, dark: bool) -> Color32 {
    let shown = shown(value);
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in shown.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Into one of twelve, rather than anywhere on the wheel. A hue taken
    // straight from the hash puts two different values a few degrees apart
    // about as often as not, and two stretches that differ have to *look*
    // different — that is the whole job. Twelve steps of the golden ratio are
    // spread as far apart as twelve hues can be.
    const WHEEL: u64 = 12;
    let hue = ((hash % WHEEL) as f32 * 0.618_034).fract();
    // Pale enough that the value written inside stays the thing being read.
    let (saturation, value) = if dark { (0.46, 0.38) } else { (0.42, 0.96) };
    egui::ecolor::Hsva::new(hue, saturation, value, 1.0).into()
}

/// The levels a track's line moves between.
struct Levels {
    high: f32,
    low: f32,
    mid: f32,
}

impl Levels {
    fn of(top: f32) -> Levels {
        let high = top + TRACK_PAD;
        let low = top + TRACK_HEIGHT - TRACK_PAD;
        Levels { high, low, mid: (high + low) / 2.0 }
    }
}

/// A single-bit signal, as a square wave.
///
/// Drawn as one polyline rather than a segment a pixel: the corners meet, the
/// verticals land where the edge was, and one stroke width applies to the whole
/// thing. That is the difference between a waveform and a row of tick marks.
fn draw_bit(
    painter: &egui::Painter,
    runs: &[Run],
    top: f32,
    left: f32,
    line: Color32,
    busy: Color32,
    bad: Color32,
) {
    let levels = Levels::of(top);
    let stroke = Stroke::new(WAVE_STROKE, line);
    let mut points: Vec<Pos2> = Vec::new();

    // Flushed whenever the line has to break — at a band, or at the end.
    let flush = |points: &mut Vec<Pos2>| {
        if points.len() > 1 {
            painter.add(egui::Shape::line(std::mem::take(points), stroke));
        } else {
            points.clear();
        }
    };

    for run in runs {
        let (x0, x1) = (left + run.x0, left + run.x1);
        if run.busy {
            flush(&mut points);
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(x0, levels.high), Pos2::new(x1, levels.low)),
                0.0,
                busy.gamma_multiply(0.5),
            );
            continue;
        }
        let Some(value) = &run.value else {
            flush(&mut points);
            continue;
        };
        if value.is_unknown() {
            flush(&mut points);
            // Hatched rather than filled, so an undriven stretch cannot be
            // mistaken for a value at either level.
            let band = Rect::from_min_max(Pos2::new(x0, levels.high), Pos2::new(x1, levels.low));
            painter.rect_filled(band, 0.0, bad.gamma_multiply(0.16));
            painter.rect_stroke(band, 0.0, Stroke::new(1.0, bad), egui::StrokeKind::Inside);
            let mut hatch = x0;
            while hatch < x1 {
                let to = (hatch + (levels.low - levels.high)).min(x1);
                painter.line_segment(
                    [
                        Pos2::new(hatch, levels.low),
                        Pos2::new(to, levels.high - (to - hatch) + (levels.low - levels.high)),
                    ],
                    Stroke::new(0.7, bad.gamma_multiply(0.7)),
                );
                hatch += 5.0;
            }
            continue;
        }
        let y = match value.as_bool() {
            Some(true) => levels.high,
            Some(false) => levels.low,
            None => levels.mid,
        };
        // The vertical to this level comes for free: the polyline goes from
        // wherever it was straight to (x0, y), which is exactly the edge.
        points.push(Pos2::new(x0, y));
        points.push(Pos2::new(x1, y));
    }
    flush(&mut points);
}

/// A bus, as a run of hexagons with the value inside.
///
/// The shoulders are what say "it changed here" — a vertical bar says the same
/// thing but reads as a signal going high, and a plain rectangle says nothing
/// at all.
#[allow(clippy::too_many_arguments)]
fn draw_bus(
    painter: &egui::Painter,
    runs: &[Run],
    top: f32,
    left: f32,
    line: Color32,
    text: Color32,
    busy: Color32,
    bad: Color32,
    dark: bool,
    names: Option<&HashMap<i64, String>>,
) {
    let levels = Levels::of(top);
    let font = FontId::monospace(BUS_TEXT);
    let stroke = Stroke::new(WAVE_STROKE, line);

    for run in runs {
        let (x0, x1) = (left + run.x0, left + run.x1);
        if run.busy {
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(x0, levels.high), Pos2::new(x1, levels.low)),
                0.0,
                busy.gamma_multiply(0.5),
            );
            continue;
        }
        let Some(value) = &run.value else { continue };
        let unknown = value.is_unknown();

        // Half the run at most, so a narrow one is a diamond rather than a
        // shape whose shoulders have crossed over each other.
        let shoulder = SHOULDER.min((x1 - x0) / 2.0);
        let hexagon = vec![
            Pos2::new(x0, levels.mid),
            Pos2::new(x0 + shoulder, levels.high),
            Pos2::new(x1 - shoulder, levels.high),
            Pos2::new(x1, levels.mid),
            Pos2::new(x1 - shoulder, levels.low),
            Pos2::new(x0 + shoulder, levels.low),
        ];
        let fill = match unknown {
            true => bad.gamma_multiply(0.16),
            false => bus_fill(value, dark),
        };
        painter.add(egui::Shape::convex_polygon(
            hexagon,
            fill,
            match unknown {
                true => Stroke::new(WAVE_STROKE, bad),
                false => stroke,
            },
        ));

        // Roughly: monospace at this size is about 0.6em a glyph. Measuring
        // exactly would want a mutable font atlas, and the only decision here
        // is whether the text fits with room to spare.
        let fits = |label: &str| {
            let wide = label.chars().count() as f32 * BUS_TEXT * 0.62;
            x1 - x0 > wide + shoulder * 2.0 + 4.0
        };
        // The name if the run is wide enough for it, the number if it is not,
        // and nothing if it is not wide enough for that either. A name is what
        // the reader came for, but an elided one cannot be told from another
        // member sharing its prefix, whereas the number is exact and the
        // readout beside the row spells the name out in full anyway.
        let named = names
            .zip(value.as_u64().and_then(|it| i64::try_from(it).ok()))
            .and_then(|(names, number)| names.get(&number))
            .cloned();
        let label = named
            .filter(|label| fits(label))
            .or_else(|| Some(shown(value)).filter(|label| fits(label)));
        if let Some(label) = label {
            painter.text(
                Pos2::new((x0 + x1) / 2.0, levels.mid),
                Align2::CENTER_CENTER,
                &label,
                font.clone(),
                text,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_annotations(
    state: &WaveState,
    painter: &egui::Painter,
    theme: &Theme,
    decode: usize,
    row: u8,
    top: f32,
    left: f32,
    plot_width: f32,
    text: Color32,
) {
    let Some(report) = state.decodes.get(decode) else { return };
    let box_top = top + 3.0;
    let box_bottom = top + TRACK_HEIGHT - 3.0;

    for annotation in report.annotations.iter().filter(|a| a.row == row) {
        let x0 = state.x_of(annotation.t_start, left);
        let x1 = state.x_of(annotation.t_end.max(annotation.t_start + 1), left).max(x0 + 2.0);
        if x1 < left || x0 > left + plot_width {
            continue;
        }
        let rect = Rect::from_min_max(
            Pos2::new(x0.max(left), box_top),
            Pos2::new(x1.min(left + plot_width), box_bottom),
        );
        let colour = match annotation.level {
            Level::Error => theme.err,
            Level::Warning => theme.warn,
            Level::Info => theme.accent,
        };
        painter.rect_stroke(rect, 2.0, Stroke::new(1.0, colour), egui::StrokeKind::Inside);
        // Only write in a box wide enough to read.
        if rect.width() > 28.0 {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                elide(&annotation.label, (rect.width() / 6.0) as usize),
                FontId::monospace(9.0),
                text,
            );
        }
    }
}

/// As much of the end of a name as fits in `room`, with a leading `…`.
///
/// From the left because a hierarchical name is a long prefix and a short tail,
/// and the tail is what tells two rows apart. Measured rather than counted:
/// the face is monospace, but the ellipsis is not one of its digits and the
/// column's width is a number of pixels, not of characters.
fn trimmed(painter: &egui::Painter, name: &str, font: &FontId, room: f32) -> String {
    trimmed_by(name, room, |text| {
        painter.layout_no_wrap(text.to_string(), font.clone(), Color32::PLACEHOLDER).size().x
    })
}

/// The decision, with the measuring handed in.
///
/// Separated so it can be read without a window: how a name is cut is a rule
/// about names, and the only thing egui contributes is how wide one is.
fn trimmed_by(name: &str, room: f32, width: impl Fn(&str) -> f32) -> String {
    if room <= 0.0 || width(name) <= room {
        return name.to_string();
    }
    let characters: Vec<char> = name.chars().collect();
    // Widen the cut until what is left fits. One character at a time is at
    // most a couple of hundred steps for a name nobody would write.
    for from in 1..characters.len() {
        let tail: String = std::iter::once('…').chain(characters[from..].iter().copied()).collect();
        if width(&tail) <= room {
            return tail;
        }
    }
    "…".to_string()
}

fn elide(text: &str, characters: usize) -> String {
    if text.chars().count() <= characters {
        return text.to_string();
    }
    text.chars().take(characters.saturating_sub(1)).collect::<String>() + "…"
}

/// One annotation the pointer is over, for a tooltip.
pub fn annotation_at(report: &DecodeReport, row: u8, time: u64) -> Option<&Annotation> {
    report
        .annotations
        .iter()
        .find(|a| a.row == row && a.t_start <= time && time <= a.t_end.max(a.t_start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(value: u64, width: u32) -> WaveValue {
        WaveValue::Bits { value, width }
    }

    /// A clock is the thing this window is worst at getting wrong, because a
    /// reader counts edges on it. Ten pixels a half period has to come out as
    /// ten runs of ten pixels, alternating — not as one blur, and not as edges
    /// rounded to the nearest column.
    #[test]
    fn a_clock_comes_out_as_a_square_wave() {
        let changes: Vec<(u64, WaveValue)> =
            (0..10u64).map(|edge| (edge * 10, bits(edge % 2, 1))).collect();
        let cut = runs(&changes, 0.0, 1.0, 100.0);

        assert_eq!(cut.len(), 10, "one run a half period: {cut:?}");
        assert!(!cut.iter().any(|run| run.busy), "nothing here is too dense: {cut:?}");
        for (index, run) in cut.iter().enumerate() {
            assert_eq!(run.x0, index as f32 * 10.0, "run {index} starts where the edge was");
            assert_eq!(
                run.value.as_ref().and_then(|value| value.as_bool()),
                // The change at t=0 is in force before the first run starts, so
                // run 0 carries it and the alternation begins low.
                Some(index % 2 == 1),
                "run {index} alternates"
            );
        }
    }

    /// Half a pixel a period is not a waveform, and drawing one of the levels
    /// would say the signal held it. The band says what actually happened.
    #[test]
    fn more_changes_than_pixels_read_as_a_band() {
        let changes: Vec<(u64, WaveValue)> = (0..100u64).map(|t| (t, bits(t % 2, 1))).collect();
        let cut = runs(&changes, 0.0, 50.0, 2.0);
        assert!(cut.iter().all(|run| run.busy), "all of it is too dense: {cut:?}");
        assert!(cut.iter().all(|run| run.value.is_none()), "a band claims no value: {cut:?}");
    }

    /// But one narrow run among wide ones is an edge, and folding it away
    /// would hide a single-cycle pulse — which is usually the bug being looked
    /// for.
    #[test]
    fn a_lone_narrow_run_stays_an_edge() {
        let changes = vec![(0, bits(0, 1)), (100, bits(1, 1)), (101, bits(0, 1))];
        let cut = runs(&changes, 0.0, 1.0, 200.0);
        assert!(!cut.iter().any(|run| run.busy), "one pulse is not a blur: {cut:?}");
        let pulse =
            cut.iter().find(|run| run.value.as_ref().and_then(|v| v.as_bool()) == Some(true));
        assert!(pulse.is_some(), "the pulse survived: {cut:?}");
    }

    /// Three spellings, because a bus is read in hex, a counter in decimal, and
    /// an `x` in neither.
    #[test]
    fn a_search_takes_the_value_however_the_reader_writes_it() {
        assert_eq!(Wanted::parse("0x1c"), Some(Wanted::Number(28)));
        assert_eq!(Wanted::parse("28"), Some(Wanted::Number(28)));
        assert_eq!(Wanted::parse("0b11100"), Some(Wanted::Number(28)));
        assert_eq!(Wanted::parse("  0X1C  "), Some(Wanted::Number(28)));
        assert_eq!(Wanted::parse(""), None);
        assert_eq!(Wanted::parse("nonsense"), None);

        // The only way to ask for an unknown, since it is not a number.
        assert_eq!(Wanted::parse("x1"), Some(Wanted::Bits("x1".into())));
        assert_eq!(Wanted::parse("0bxx"), Some(Wanted::Bits("xx".into())));
    }

    /// A number matches a value however wide it is; a pattern matches the low
    /// bits, so a reader can ask about the end of a bus they do not know all of.
    #[test]
    fn a_search_matches_the_value_and_not_its_spelling() {
        let bus = WaveValue::Bits { value: 28, width: 8 };
        assert!(Wanted::Number(28).matches(&bus));
        assert!(!Wanted::Number(27).matches(&bus));

        let unknown = WaveValue::Unknown { bits: "0001x100".into() };
        assert!(!Wanted::Number(28).matches(&unknown), "an x is never a number");
        assert!(Wanted::Bits("x100".into()).matches(&unknown));
        assert!(!Wanted::Bits("x101".into()).matches(&unknown));
    }

    /// The picker is a tree because a dump is one; the paths are all that is
    /// left of the scopes the file wrote.
    #[test]
    fn dotted_paths_come_back_as_the_hierarchy_they_describe() {
        let paths = ["tb.dut.u_rx.data", "tb.dut.clk", "tb.done", "tb.dut.u_rx.valid"];
        let tree = Scope::of(paths.into_iter(), |_| true);

        // One root scope, `tb`, since every path starts there.
        assert_eq!(tree.children.len(), 1);
        let tb = &tree.children[0];
        assert_eq!(tb.name, "tb");
        assert_eq!(tb.vars.len(), 1, "`done` sits directly in tb");
        assert_eq!(tb.vars[0].name, "done");

        let dut = tb.children.iter().find(|scope| scope.name == "dut").expect("tb.dut");
        assert_eq!(dut.vars.len(), 1, "`clk` sits in dut");
        let rx = dut.children.iter().find(|scope| scope.name == "u_rx").expect("tb.dut.u_rx");
        let names: Vec<&str> = rx.vars.iter().map(|leaf| leaf.name.as_str()).collect();
        assert_eq!(names, ["data", "valid"], "sorted, so the list does not shuffle");

        assert_eq!(tree.total(), 4, "every leaf is counted, however deep");
    }

    /// A signal the design does not have is kept and marked, not dropped. It is
    /// usually the testbench's own, and it is often the one being read against.
    #[test]
    fn a_variable_the_design_lacks_is_kept_and_said_to_be_missing() {
        let paths = ["tb.dut.clk", "tb.stimulus_done"];
        let tree = Scope::of(paths.into_iter(), |path| path.starts_with("tb.dut."));

        let tb = &tree.children[0];
        let own = tb.vars.iter().find(|leaf| leaf.name == "stimulus_done").expect("kept");
        assert!(!own.known, "the design does not declare it");

        let dut = tb.children.iter().find(|scope| scope.name == "dut").expect("tb.dut");
        assert!(dut.vars[0].known, "this one the design does have");
    }

    /// A ruler whose ticks fall on 137, 274, 411 is one nobody can read a
    /// position off.
    #[test]
    fn the_axis_only_marks_round_numbers() {
        assert_eq!(nice_step(137.0), 200.0);
        assert_eq!(nice_step(1.0), 1.0);
        assert_eq!(nice_step(3.0), 5.0);
        assert_eq!(nice_step(0.3), 0.5);
        assert_eq!(nice_step(6.0), 10.0);
        // Nothing sensible to do with these, and a ruler still has to be drawn.
        assert_eq!(nice_step(0.0), 1.0);
        assert_eq!(nice_step(f64::NAN), 1.0);
    }

    /// Before the dump begins the signal held nothing, which is not the same
    /// as having held zero.
    #[test]
    fn a_stretch_before_the_first_change_holds_nothing() {
        let changes = vec![(100, bits(1, 1))];
        let cut = runs(&changes, 0.0, 10.0, 30.0);
        assert_eq!(cut.first().and_then(|run| run.value.clone()), None, "{cut:?}");
    }

    #[test]
    fn an_undriven_stretch_is_marked_rather_than_drawn_as_zero() {
        let changes = vec![(0, WaveValue::Unknown { bits: "xxxx".into() }), (50, bits(3, 4))];
        let cut = runs(&changes, 0.0, 10.0, 60.0);
        let undriven = cut.first().expect("a first run");
        assert!(
            undriven.value.as_ref().is_some_and(|value| value.is_unknown()),
            "the x survives to the drawing: {cut:?}"
        );
        assert!(
            cut.last().and_then(|run| run.value.as_ref()).is_some_and(|v| !v.is_unknown()),
            "and stops where it stops: {cut:?}"
        );
    }

    /// The value in force at the left edge comes from before it, so scrolling
    /// into the middle of a dump does not start blank.
    #[test]
    fn a_view_starting_mid_dump_carries_the_value_forward() {
        let changes = vec![(0, bits(0, 8)), (10, bits(0xAB, 8)), (1000, bits(0xCD, 8))];
        let cut = runs(&changes, 500.0, 10.0, 20.0);
        assert_eq!(
            cut.first().and_then(|run| run.value.as_ref()).and_then(|v| v.as_u64()),
            Some(0xAB),
            "{cut:?}"
        );
    }

    /// A run has to start where the change did, not at the nearest whole
    /// pixel. This is the whole reason the drawing stopped going through bins:
    /// rounding every edge to a column is what turned a clock into ticks.
    #[test]
    fn an_edge_lands_where_the_change_was_not_on_a_column() {
        let changes = vec![(0, bits(0, 1)), (25, bits(1, 1))];
        // 10 units a pixel, so the change at 25 is two and a half pixels in.
        let cut = runs(&changes, 0.0, 10.0, 10.0);
        assert_eq!(cut[0].x1, 2.5, "the edge keeps its fraction: {cut:?}");
        assert_eq!(cut[1].x0, 2.5, "and the next run starts there: {cut:?}");
    }

    /// A move is reported once. Reporting it every frame would send the source
    /// view chasing the same line forever, and reporting it never would leave
    /// selecting a track doing nothing outside the panel.
    #[test]
    fn a_selection_is_reported_once_and_then_not_again() {
        let mut state = opened();
        assert_eq!(newly_selected(&mut state), None, "nothing selected, nothing to say");

        state.select_only(0);
        let first = newly_selected(&mut state);
        assert!(first.is_some(), "the first track records a wire of the design");
        assert_eq!(newly_selected(&mut state), None, "and it is not said twice");

        state.select_only(1);
        assert!(newly_selected(&mut state).is_some(), "a move is a new thing to say");
    }

    /// Any variable of the opened fixture, for a track built by hand.
    fn some_var(state: &WaveState) -> WaveVar {
        match state.tracks.first() {
            Some(Track::Signal { var, .. }) => *var,
            _ => panic!("the fixture opens with signal tracks"),
        }
    }

    /// A dump of a real design is one prefix repeated: the hierarchy is right
    /// there and the eye has to read it out of twenty dotted strings. Laid out
    /// as a tree it is a line a scope and a signal under it.
    #[test]
    fn signals_are_laid_out_under_the_scopes_they_live_in() {
        let mut state = opened();
        let names: Vec<String> = state.shown_names();
        assert!(names.iter().all(|name| !name.contains('.')), "leaves under a scope: {names:?}");

        let layout = state.layout();
        let scopes: Vec<(&str, usize, usize)> = layout
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Scope { label, depth, holds, .. } => {
                    Some((label.as_str(), *depth, holds.len()))
                }
                _ => None,
            })
            .collect();
        // `tb` holds everything, `dut` holds the same signals one level in.
        assert_eq!(scopes.len(), 2, "two scopes over one module's dump: {scopes:?}");
        assert_eq!(scopes[0].0, "tb");
        assert_eq!(scopes[0].1, 0);
        assert_eq!(scopes[1].0, "dut");
        assert_eq!(scopes[1].1, 1);
        assert_eq!(scopes[0].2, state.tracks.len(), "the outer one holds them all");
        assert_eq!(scopes[1].2, state.tracks.len());

        // Every track is on a line, and every line's track is that track.
        for index in 0..state.tracks.len() {
            let line = layout.line_of[index].expect("nothing is folded, so every track is drawn");
            assert_eq!(layout.track_at(line), Some(index), "track {index} is not on its line");
        }
        assert_eq!(layout.lines(), state.tracks.len() + 2, "a line each, plus the two scopes");

        // Flat again, and there is nothing but tracks.
        state.names = Names::Path;
        let flat = state.layout();
        assert_eq!(flat.lines(), state.tracks.len());
        assert!(state.shown_names().iter().all(|name| name.starts_with("tb.")), "whole paths");
    }

    /// Insertion order is kept, so a reader who interleaves two scopes sees
    /// each one twice. That is a true picture of the list they built —
    /// reordering rows to tidy it would move a waveform somebody put where
    /// they wanted it.
    #[test]
    fn a_scope_the_reader_came_back_to_is_drawn_twice() {
        let mut state = opened();
        // A real variable, because a track holds one; which one is beside the
        // point, since the column is laid out from the names.
        let var = some_var(&state);
        state.tracks.clear();
        for path in ["tb.u_rx.data", "tb.u_tx.data", "tb.u_rx.valid"] {
            state.tracks.push(Track::Signal {
                var,
                name: path.to_string(),
                width: 1,
                signal: None,
            });
        }

        let layout = state.layout();
        let scopes: Vec<&str> = layout
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Scope { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(scopes, ["tb", "u_rx", "u_tx", "u_rx"], "{scopes:?}");
    }

    /// A decoder's lane is named by this program, not by the dump. There is no
    /// scope above it to draw, and a rule that split it on its dots would be
    /// cutting up a phrase somebody wrote to be read.
    #[test]
    fn a_lane_that_is_not_a_signal_is_left_at_the_edge() {
        let mut state = opened();
        let var = some_var(&state);
        state.tracks.clear();
        state.tracks.push(Track::Signal {
            var,
            name: "tb.dut.clk".to_string(),
            width: 1,
            signal: None,
        });
        state.tracks.push(Track::Decoded { decode: 0, row: 0, name: "AXI4 · write".to_string() });

        let layout = state.layout();
        assert_eq!(
            layout.rows.iter().filter(|row| matches!(row, Row::Scope { .. })).count(),
            2,
            "the signal's two scopes, and none invented for the lane"
        );
        let depths: Vec<usize> = layout
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Track { depth, .. } => Some(*depth),
                _ => None,
            })
            .collect();
        assert_eq!(depths, [2, 0], "the signal is two in, the lane is at the edge");
        assert_eq!(state.shown_names()[1], "AXI4 · write", "and keeps its whole name");
    }

    /// Folding a scope takes its lines away and leaves the tracks alone. The
    /// panel is what is being changed, not the list.
    #[test]
    fn folding_a_scope_hides_its_lines_and_keeps_its_tracks() {
        let mut state = opened();
        let before = state.layout().lines();
        let tracks = state.tracks.len();

        state.fold("tb.dut");
        assert!(state.is_shut("tb.dut"));

        let folded = state.layout();
        assert_eq!(state.tracks.len(), tracks, "the tracks stayed on the panel");
        assert!(folded.lines() < before, "and their lines went: {} of {before}", folded.lines());
        assert!(
            folded.line_of.iter().all(Option::is_none),
            "every signal was under it, so none is drawn"
        );
        // The scope itself is still there to be opened again, and still says
        // how much it is holding.
        let shut: Vec<(&str, usize, bool)> = folded
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Scope { path, holds, shut, .. } => Some((path.as_str(), holds.len(), *shut)),
                _ => None,
            })
            .collect();
        assert_eq!(shut, [("tb", tracks, false), ("tb.dut", tracks, true)], "{shut:?}");

        state.fold("tb.dut");
        assert_eq!(state.layout().lines(), before, "and the same click opens it again");
    }

    /// A scope shut over another takes it with it, whatever the inner one says
    /// about itself — to the reader there is one fold, and opening it should
    /// put back what was there before.
    #[test]
    fn a_fold_higher_up_hides_what_is_under_it() {
        let mut state = opened();
        state.fold("tb");

        let layout = state.layout();
        assert_eq!(layout.lines(), 1, "the outermost scope, and nothing else: {:?}", layout.rows);
        assert!(matches!(layout.rows[0], Row::Scope { ref path, .. } if path == "tb"));
    }

    /// A fold is a way of looking at the list, not a way of hiding an answer.
    /// `first difference` and adding a signal that is already there both point
    /// at a row, and pointing at one nobody can see says nothing at all.
    #[test]
    fn pointing_at_a_row_opens_whatever_was_folded_over_it() {
        let mut state = opened();
        state.fold("tb.dut");
        assert!(state.layout().line_of.iter().all(Option::is_none), "folded away");

        state.select_only(1);

        assert!(!state.is_shut("tb.dut"), "the fold over it opened");
        assert!(state.layout().line_of[1].is_some(), "and the row is drawn again");
        assert_eq!(state.selected(), Some(1));
    }

    /// Clicking a scope picks everything under it, which is what makes a
    /// module's worth of rows something a reader can take off in one go. The
    /// same click with `ctrl` takes them back out again.
    #[test]
    fn clicking_a_scope_picks_everything_under_it() {
        let mut state = opened();
        let all: Vec<usize> = (0..state.tracks.len()).collect();

        state.pick_all(&all, false);
        assert_eq!(state.selected_rows(), all, "the whole scope");

        state.pick_all(&all, true);
        assert!(state.selected_rows().is_empty(), "and the same click again lets it go");

        state.click_row(0, false, false);
        state.pick_all(&[1, 2], true);
        assert_eq!(state.selected_rows(), vec![0, 1, 2], "ctrl adds a scope to what was picked");
    }

    /// Ctrl takes a row in or out without disturbing the rest, which is what
    /// makes a selection something a reader builds rather than replaces.
    #[test]
    fn ctrl_clicking_takes_a_row_in_and_out_of_the_selection() {
        let mut state = opened();
        state.click_row(0, false, false);
        state.click_row(2, true, false);
        assert_eq!(state.selected_rows(), vec![0, 2]);
        assert_eq!(state.selected(), Some(2), "the newest is the anchor");

        state.click_row(0, true, false);
        assert_eq!(state.selected_rows(), vec![2], "and out again");

        state.click_row(1, false, false);
        assert_eq!(state.selected_rows(), vec![1], "a plain click means only this one");
    }

    /// Shift reaches from the anchor, and the anchor stays where the reader
    /// put it — so a second reach is drawn from the same place rather than
    /// from wherever the first one stopped.
    #[test]
    fn shift_clicking_reaches_from_the_anchor_and_leaves_it_there() {
        let mut state = opened();
        state.click_row(1, false, false);
        state.click_row(3, false, true);
        assert_eq!(state.selected_rows(), vec![1, 2, 3]);
        assert_eq!(state.selected(), Some(1), "the anchor is still where it was");

        // Backwards from the same anchor, which is the case that goes wrong
        // when the anchor is allowed to travel.
        state.click_row(0, false, true);
        assert_eq!(state.selected_rows(), vec![0, 1]);
        assert_eq!(state.selected(), Some(1));
    }

    /// The point of the whole thing: several rows go in one gesture. Taking
    /// them highest-first is what keeps the right ones going — from the front,
    /// every position after the first is already stale.
    #[test]
    fn every_picked_row_comes_off_at_once() {
        let mut state = opened();
        let names: Vec<String> = state
            .tracks
            .iter()
            .map(|track| match track {
                Track::Signal { name, .. } => name.clone(),
                _ => String::new(),
            })
            .collect();
        assert!(names.len() >= 4, "the fixture opens with rows to take off: {names:?}");
        let (first, last) = (names[0].clone(), names[names.len() - 1].clone());

        state.click_row(1, false, false);
        state.click_row(2, true, false);
        let taken = state.remove_selected();

        assert_eq!(taken, 2);
        assert_eq!(state.tracks.len(), names.len() - 2);
        assert!(state.selected().is_none(), "nothing is pointed at afterwards");
        let left: Vec<String> = state
            .tracks
            .iter()
            .map(|track| match track {
                Track::Signal { name, .. } => name.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(left.first(), Some(&first), "the rows before the picked ones stayed");
        assert_eq!(left.last(), Some(&last), "and so did the ones after");
        assert!(!left.contains(&names[1]), "the picked rows went: {left:?}");
        assert!(!left.contains(&names[2]), "the picked rows went: {left:?}");
    }

    /// Taking one row off with the × moves every picked row after it, because
    /// a selection is a list of positions and those positions have changed.
    #[test]
    fn removing_one_row_moves_the_picked_rows_after_it() {
        let mut state = opened();
        state.click_row(2, false, false);
        state.click_row(3, true, false);

        state.remove(0);

        assert_eq!(state.selected_rows(), vec![1, 2], "each moved up by the one that went");
    }

    // ------------------------------------------ putting the rows in order ---

    /// Every track's whole name, in the order the panel holds them.
    fn order(state: &WaveState) -> Vec<String> {
        state
            .tracks
            .iter()
            .map(|track| match track {
                Track::Signal { name, .. }
                | Track::Decoded { name, .. }
                | Track::Stage { name, .. } => name.clone(),
            })
            .collect()
    }

    /// The order of the panel is an argument the reader is making — a clock,
    /// then the valid, then the data it qualifies — and nothing else in this
    /// program can make it for them.
    #[test]
    fn a_row_moves_to_where_it_was_dropped() {
        let mut state = opened();
        let before = order(&state);
        assert!(before.len() >= 4, "the fixture opens with rows to shuffle: {before:?}");
        let last = before.len() - 1;

        assert!(state.move_rows(&[last], 0), "the bottom row to the top");

        let after = order(&state);
        assert_eq!(after[0], before[last], "it landed in front of everything");
        assert_eq!(&after[1..], &before[..last], "and the rest kept the order they had");
    }

    /// A drop that landed where it started is not a move, and saying it was
    /// would put a line in the status bar every time somebody's click travelled
    /// a pixel — which egui calls a drag.
    #[test]
    fn a_drop_where_it_started_moves_nothing() {
        let mut state = opened();
        let before = order(&state);

        assert!(!state.move_rows(&[1], 1), "in front of itself is where it already is");
        assert!(!state.move_rows(&[1], 2), "and so is in front of the row below it");
        assert!(!state.move_rows(&[], 0), "nothing picked up, nothing put down");
        assert!(!state.move_rows(&[99], 0), "a row that is not there is not a row");

        assert_eq!(order(&state), before);
    }

    /// Rows picked from three places arrive together. A reader who picked them
    /// together asked for them together, and the alternative — each keeping its
    /// distance from the others — is an order nobody can predict from the
    /// gesture they made.
    #[test]
    fn scattered_rows_arrive_as_one_block() {
        let mut state = opened();
        let before = order(&state);
        let end = before.len();

        assert!(state.move_rows(&[0, 2], end), "two rows, from either side of a third");

        let after = order(&state);
        assert_eq!(
            &after[end - 2..],
            &[before[0].clone(), before[2].clone()],
            "both at the end, in the order they were in: {after:?}"
        );
        assert_eq!(after[0], before[1], "and what was between them closed up");
    }

    /// The selection is a list of positions, and a move changes every one of
    /// them. Pointing at the wrong row afterwards is worse than pointing at
    /// none, because it looks like it worked.
    #[test]
    fn the_picked_rows_are_still_the_picked_rows_afterwards() {
        let mut state = opened();
        let names = order(&state);
        let last = names.len() - 1;
        state.click_row(last, false, false);
        state.click_row(0, true, false);

        assert!(state.move_rows(&[last], 0));

        assert_eq!(state.selected_rows(), vec![0, 1], "the moved row, and the one it displaced");
        assert_eq!(order(&state)[0], names[last], "and the row that moved is the one it was");
    }

    /// A row that only changed place is not a row somebody just clicked. Saying
    /// it was would pull the source and the diagram to a signal nobody chose.
    #[test]
    fn moving_the_anchor_does_not_read_as_a_new_selection() {
        let mut state = opened();
        let last = state.tracks.len() - 1;
        state.select_only(last);
        assert!(newly_selected(&mut state).is_some(), "picked, and said once");

        assert!(state.move_rows(&[last], 0));

        assert_eq!(state.selected(), Some(0), "the same row, at the front now");
        assert_eq!(newly_selected(&mut state), None, "and nothing new to say about it");
    }

    /// What the drag does, for a hand already on the keyboard. Both ends stop
    /// rather than wrap: a row that leapt from the bottom to the top would look
    /// like one that had been lost.
    #[test]
    fn the_keyboard_moves_a_row_one_place_and_stops_at_the_ends() {
        let mut state = opened();
        let names = order(&state);
        let last = names.len() - 1;

        state.select_only(0);
        assert!(!state.move_selected(false), "nothing above the first row");
        assert!(state.move_selected(true), "and one place down from it");
        assert_eq!(order(&state), {
            let mut wanted = names.clone();
            wanted.swap(0, 1);
            wanted
        });
        assert_eq!(state.selected(), Some(1), "the selection went with it");

        state.select_only(last);
        assert!(!state.move_selected(true), "nothing below the last row");

        state.select_none();
        assert!(!state.move_selected(true), "and nothing picked is nothing to move");
    }

    /// A module's header line stands for the rows under it, so a drop above it
    /// goes in front of its first signal — not past the ones it is hiding,
    /// which are not on screen to be aimed past.
    #[test]
    fn a_drop_above_a_folded_module_lands_in_front_of_it() {
        let mut state = opened();
        let var = some_var(&state);
        state.tracks.clear();
        for path in ["tb.top_clk", "tb.u_rx.data", "tb.u_rx.valid", "tb.done"] {
            state.tracks.push(Track::Signal {
                var,
                name: path.to_string(),
                width: 1,
                signal: None,
            });
        }
        state.fold("tb.u_rx");

        // tb · top_clk · u_rx (shut) · done
        let layout = state.layout();
        assert_eq!(layout.lines(), 4, "{:?}", layout.rows);
        assert_eq!(layout.insertion(0), 0, "above everything");
        assert_eq!(layout.insertion(2), 1, "the folded module's own first row");
        assert_eq!(layout.insertion(3), 3, "past it, which is the row after the two it holds");
        assert_eq!(layout.insertion(4), 4, "below the last line is the end of the list");
        assert_eq!(layout.insertion(99), 4, "and so is anywhere past that");
    }

    /// A row taken off is not left in the selection, or the next bulk remove
    /// would take a row the reader never picked.
    #[test]
    fn a_row_taken_off_is_not_still_picked() {
        let mut state = opened();
        state.click_row(1, false, false);
        state.click_row(2, true, false);

        state.remove(1);

        assert_eq!(state.selected_rows(), vec![1], "what was row 2, and nothing else");
    }

    /// Adding in bulk says what became of each one. Silently skipping the
    /// duplicates would let a reader ask for eight, get three, and be told
    /// nothing about the other five.
    #[test]
    fn adding_several_says_how_many_were_new() {
        let mut state = opened();
        let paths: Vec<String> = state
            .dump
            .vars()
            .map(|(path, _)| path.to_string())
            .filter(|path| path.contains("in_") || path.contains("out_"))
            .take(4)
            .collect();
        assert!(!paths.is_empty(), "the fixture has signals to add");

        let before = state.tracks.len();
        let said = state.add_all(&paths);
        assert!(said.starts_with("added "), "{said}");
        assert!(state.tracks.len() > before || said.contains("already"), "{said}");

        // The same ones again: none are new, and it says so rather than
        // reporting a second success.
        let again = state.add_all(&paths);
        assert!(again.contains("already on the panel"), "{again}");
        assert!(again.starts_with("added 0"), "{again}");
    }

    /// More than a panel can be read at is refused rather than trimmed. A
    /// reader who asked for four hundred and silently got sixty-four has a
    /// panel that is wrong in a way nothing on it says.
    #[test]
    fn asking_for_more_rows_than_a_panel_can_hold_is_refused_out_loud() {
        let mut state = opened();
        let before = state.tracks.len();
        let too_many: Vec<String> =
            (0..ADD_AT_ONCE + 1).map(|n| format!("tb.made_up{n}")).collect();

        let said = state.add_all(&too_many);

        assert_eq!(state.tracks.len(), before, "nothing was put on: {said}");
        assert!(said.contains(&(ADD_AT_ONCE + 1).to_string()), "it says how many: {said}");
        assert!(said.contains("Narrow the search"), "and what to do instead: {said}");
    }

    /// Zooming to a stretch puts its ends at the edges of the plot. That is
    /// the whole promise of the gesture: what was swept is what is shown.
    #[test]
    fn zooming_to_a_stretch_puts_its_ends_at_the_edges() {
        let mut state = opened();
        state.zoom_to(200.0, 700.0, 500.0);

        assert!((state.x_of(200, 0.0) - 0.0).abs() < 1.0, "the near end is the left edge");
        assert!((state.x_of(700, 0.0) - 500.0).abs() < 1.0, "and the far end is the right");
    }

    /// Dragged right to left means the same stretch. A reader sweeping
    /// backwards is pointing at the same piece of time.
    #[test]
    fn a_stretch_swept_backwards_is_the_same_stretch() {
        let mut forwards = opened();
        let mut backwards = opened();
        forwards.zoom_to(200.0, 700.0, 500.0);
        backwards.zoom_to(700.0, 200.0, 500.0);

        assert_eq!(forwards.t_left, backwards.t_left);
        assert_eq!(forwards.per_px, backwards.per_px);
    }

    /// An instant has no scale, and dividing by its width would give one
    /// anyway. The gesture that produces one means something else.
    #[test]
    fn zooming_to_a_single_instant_changes_nothing() {
        let mut state = opened();
        let (was_left, was_scale) = (state.t_left, state.per_px);
        state.zoom_to(400.0, 400.0, 500.0);

        assert_eq!(state.t_left, was_left);
        assert_eq!(state.per_px, was_scale);
    }

    /// A dump's names are one long prefix and a short distinguishing tail, so
    /// the end is the part that tells one row from the next. Cutting the end
    /// off leaves a column of `tb_pipeline_demo.u_dut.c…` twelve times over.
    #[test]
    fn a_name_too_long_for_its_column_keeps_its_end() {
        // A monospace face, six pixels a character, ellipsis included.
        let width = |text: &str| text.chars().count() as f32 * 6.0;

        assert_eq!(trimmed_by("clk", 120.0, width), "clk", "what fits is left alone");

        let long = "tb_pipeline_demo.u_dut.core_clk";
        let cut = trimmed_by(long, 120.0, width);
        assert!(cut.starts_with('…'), "cut at the front: {cut}");
        assert!(cut.ends_with("core_clk"), "and the tail survives: {cut}");
        assert!(width(&cut) <= 120.0, "and it fits: {cut}");

        // No room at all is still an answer, not a panic or a full name drawn
        // over the waveform.
        assert_eq!(trimmed_by(long, 0.0, width), long, "no column, no cut");
        assert_eq!(trimmed_by(long, 3.0, width), "…");
    }

    /// A span is read in nanoseconds, and a short one has to survive the
    /// reading. A picosecond dump of a fast clock has half periods well under a
    /// nanosecond, and `Δ 0 ns` is worse than no answer at all.
    #[test]
    fn a_span_is_said_in_the_units_a_reader_measures_in() {
        let state = state_machine();
        // The hand-written dump is in nanoseconds, so a tick is a nanosecond.
        assert_eq!(state.span_words(40), "Δ 40 ns");
        assert_eq!(state.span_words(0), "Δ 0.000 ns", "and a short one keeps its decimals");
    }

    /// Markers are kept sorted, which is what lets the ruler write a distance
    /// between each neighbouring pair. Dropped out of order they would pair up
    /// across each other and measure spans nobody asked about.
    #[test]
    fn markers_stay_in_order_however_they_are_dropped() {
        let mut state = state_machine();
        for at in [300u64, 100, 200] {
            state.toggle_marker(at);
        }
        assert_eq!(state.markers(), [100, 200, 300]);

        let spans: Vec<u64> = state.markers().windows(2).map(|pair| pair[1] - pair[0]).collect();
        assert_eq!(spans, [100, 100], "neighbours, not every combination");

        // And the same moment twice takes it back down rather than stacking a
        // second flag nobody can see behind the first.
        state.toggle_marker(200);
        assert_eq!(state.markers(), [100, 300]);
    }

    /// A marker is dropped in about the right place and then wanted exactly on
    /// an edge. Taking it down to put it back a pixel along is two gestures for
    /// one correction, so a flag drags.
    #[test]
    fn a_marker_can_be_slid_to_another_moment() {
        let mut state = state_machine();
        for at in [100u64, 200, 300] {
            state.toggle_marker(at);
        }

        state.move_marker(1, 250);
        assert_eq!(state.markers(), [100, 250, 300]);

        // Dragged past a neighbour the list stays sorted, which is what lets
        // the ruler write a distance between each neighbouring pair.
        state.move_marker(1, 350);
        assert_eq!(state.markers(), [100, 300, 350]);
    }

    /// Two flags at one moment measure nothing, and one of them would be
    /// invisible behind the other and still in the way.
    #[test]
    fn a_marker_dragged_onto_another_merges_with_it() {
        let mut state = state_machine();
        for at in [100u64, 200] {
            state.toggle_marker(at);
        }
        state.move_marker(0, 200);
        assert_eq!(state.markers(), [200]);
    }

    /// The measurement beside the toolbar and the one written on the ruler are
    /// the same words about the same distance. Two spellings of one span read
    /// as two different measurements.
    #[test]
    fn the_toolbar_and_the_ruler_say_a_span_the_same_way() {
        let mut state = state_machine();
        state.toggle_marker(10);
        state.cursor = Some(30);
        assert_eq!(state.measure().as_deref(), Some(state.span_words(20).as_str()));
    }

    /// The thumb says what fraction is on screen and where in the recording it
    /// is. Both halves matter: the width answers "how much of this am I
    /// looking at", which the ruler's numbers never say out loud.
    #[test]
    fn the_thumb_says_how_much_is_on_screen_and_where() {
        // A thousand ticks recorded, a hundred of them across a 400px plot.
        let bar = Bar::of(0.25, 400.0, 1000, 800.0);
        assert!((bar.thumb - 80.0).abs() < 0.5, "a tenth of the bar: {bar:?}");
        assert_eq!(bar.offset(0.0), 0.0, "at the start, at the left");
        assert!((bar.offset(900.0) - bar.travel).abs() < 0.5, "at the end, at the right");
        assert!((bar.offset(450.0) - bar.travel / 2.0).abs() < 0.5, "halfway, halfway");
    }

    /// Which half of the panel the pointer is over decides what a notch of the
    /// wheel does. Over the names it is a list and the wheel scrolls it; over
    /// the waveforms it is a picture of time and the wheel scales it. The line
    /// between them is the one the reader can already see.
    #[test]
    fn the_wheel_moves_the_list_only_over_the_names() {
        let left = 40.0;
        let at = |x: f32| Some(egui::Pos2::new(x, 100.0));

        assert!(wheel_moves_the_list(at(left), left), "the first pixel of the names");
        assert!(wheel_moves_the_list(at(left + NAME_WIDTH - 1.0), left), "the last one");
        assert!(!wheel_moves_the_list(at(left + NAME_WIDTH), left), "where the waveforms start");
        assert!(!wheel_moves_the_list(at(left + 900.0), left), "and out across them");
        // Left of the panel altogether: another pane, and not this one's wheel
        // to take. Nothing is hovered there, so nothing moves either way.
        assert!(wheel_moves_the_list(at(left - 10.0), left), "left of the panel is not the plot");
        assert!(!wheel_moves_the_list(None, left), "and with no pointer, neither");
    }

    /// The wheel used to zoom out until the dump was a bright line in a field
    /// of empty grid, and a pan could leave it behind altogether. There is
    /// nothing out there to look at, so there is no reason to be able to go.
    #[test]
    fn the_view_cannot_wander_off_the_recording() {
        let mut state = opened();
        let plot = 1000.0f32;
        let end = state.dump.max_time().max(1) as f64;

        // Zoomed out a thousandfold.
        state.per_px = end / f64::from(plot) * 1000.0;
        state.hold_in_view(plot);
        let shown = state.per_px * f64::from(plot);
        assert!(shown <= end * 1.25 + 1.0, "the recording and a quarter, no more: {shown}");

        // Panned far past the end.
        state.fit(plot);
        state.t_left = end * 50.0;
        state.hold_in_view(plot);
        assert!(state.t_left <= end * 1.25, "the start stays in reach: {}", state.t_left);

        // And time before the recording is still nothing to show.
        state.t_left = -end;
        state.hold_in_view(plot);
        assert_eq!(state.t_left, 0.0);
    }

    /// A quarter past the end is deliberate: the last edge should not sit
    /// against the frame, and the gap is what says the recording stops there.
    #[test]
    fn a_little_room_is_left_past_the_end() {
        let mut state = opened();
        let plot = 1000.0f32;
        let end = state.dump.max_time().max(1) as f64;

        // Zoomed in, so the window is small next to the slack.
        state.per_px = end / f64::from(plot) / 8.0;
        state.t_left = end;
        state.hold_in_view(plot);
        assert!(state.t_left >= end, "the end can be brought to the left edge: {}", state.t_left);
    }

    /// Fitting shows the recording and no more, so the thumb fills the bar and
    /// the very next notch of the wheel visibly shrinks it.
    ///
    /// Sized against a guessed 800 pixels on an 1150-pixel plot, `fit` showed
    /// half again as much time as the recording had: a third of the panel spent
    /// on nothing, and a thumb clamped at full width through the first few
    /// notches of zooming in — which looks exactly like a scrollbar that does
    /// not respond to the wheel.
    #[test]
    fn fitting_fills_the_bar_and_the_next_zoom_shrinks_it() {
        let mut state = opened();
        let plot = 1150.0;
        state.fit(plot);

        let bar = Bar::of(state.per_px, plot, state.dump.max_time(), plot);
        assert_eq!(bar.thumb, plot, "the whole recording is the whole bar: {bar:?}");
        assert!(!bar.scrolls(), "and there is nowhere left to scroll");

        state.per_px /= 2.0;
        let closer = Bar::of(state.per_px, plot, state.dump.max_time(), plot);
        assert!((closer.thumb - plot / 2.0).abs() < 1.0, "half as much shown: {closer:?}");
        assert!(closer.scrolls(), "and now there is somewhere to go");
    }

    /// A dump that has just been opened has not met a plot yet, so it asks to
    /// be fitted rather than guessing a width to be fitted against.
    #[test]
    fn a_dump_just_opened_asks_to_be_sized_against_the_plot() {
        assert!(opened().refit, "the width is not known until the panel is laid out");
    }

    /// Dragging the thumb moves the view and nothing else. Every other gesture
    /// on this panel changes the scale, so the one that does not has to keep
    /// its promise exactly.
    #[test]
    fn dragging_the_thumb_moves_the_view_and_not_the_scale() {
        let bar = Bar::of(0.25, 400.0, 1000, 800.0);
        let was = bar.offset(0.0);
        let moved = bar.t_left(was + bar.travel / 2.0);

        assert!((moved - 450.0).abs() < 1.0, "halfway along is halfway through: {moved}");
        // Round trip: where the thumb lands is where it says it is.
        assert!((bar.offset(moved) - bar.travel / 2.0).abs() < 0.5);
    }

    /// Time before the recording and time after it are both nothing to look at,
    /// and a bar that let the view slide into either would put the whole
    /// waveform against one edge with blank beside it.
    #[test]
    fn the_view_never_scrolls_past_either_end() {
        let bar = Bar::of(0.25, 400.0, 1000, 800.0);
        assert_eq!(bar.t_left(-500.0), 0.0, "no earlier than the start");
        assert!(
            (bar.t_left(bar.travel + 500.0) - 900.0).abs() < 1.0,
            "and no later than one window short of the end"
        );
    }

    /// A recording that fits has nowhere to go, and a bar that moved anyway
    /// would be a control that lies about what it does.
    #[test]
    fn a_recording_that_fits_entirely_does_not_scroll() {
        let bar = Bar::of(4.0, 400.0, 1000, 800.0);
        assert!(!bar.scrolls(), "all 1000 ticks are on screen: {bar:?}");
        assert_eq!(bar.thumb, 800.0, "so the thumb fills the bar");
        assert_eq!(bar.t_left(400.0), 0.0, "and dragging it changes nothing");
    }

    /// Zoomed far in the thumb would be a pixel wide, which is a control that
    /// exists without being usable.
    #[test]
    fn the_thumb_stays_wide_enough_to_grab() {
        let bar = Bar::of(0.001, 400.0, 1_000_000, 800.0);
        assert_eq!(bar.thumb, THUMB_MIN, "held at the floor: {bar:?}");
        assert!(bar.scrolls(), "and it still moves the view");
        assert!(bar.t_left(bar.travel) > 0.0);
    }

    /// Pressing the groove puts that moment in the middle of the plot. Against
    /// the left edge would show half of whatever the reader was pointing at.
    #[test]
    fn pressing_the_bar_puts_that_moment_in_the_middle() {
        let bar = Bar::of(0.25, 400.0, 1000, 800.0);
        // Halfway along the bar is tick 500; a hundred-tick window centred on
        // it starts at 450.
        assert!((bar.centred(400.0, 800.0) - 450.0).abs() < 1.0);
        assert_eq!(bar.centred(0.0, 800.0), 0.0, "clamped at the start");
        assert!((bar.centred(800.0, 800.0) - 900.0).abs() < 1.0, "and at the end");
    }

    /// A sweep can begin before the recording does — the plot has margin at
    /// the left and a hand overshoots. Showing time that never happened would
    /// put the start of the dump somewhere in the middle of the window.
    #[test]
    fn a_sweep_off_the_front_of_the_recording_starts_at_zero() {
        let mut state = opened();
        state.zoom_to(-300.0, 500.0, 500.0);

        assert_eq!(state.t_left, 0.0, "the view starts where the recording does");
        assert!((state.x_of(500, 0.0) - 500.0).abs() < 1.0, "the far end is still the right one");
        assert!(
            state.per_px * 500.0 < 800.0,
            "less time is shown than was swept, because part of what was swept is not there"
        );
    }

    /// The difference between sweeping and pressing is a threshold, and it is
    /// worth being able to see the far side of: below it a trackpad click
    /// routinely travels a pixel or two, and a viewer that zoomed to a
    /// two-pixel window every time somebody set the cursor would be unusable.
    #[test]
    fn a_drag_too_short_to_be_a_sweep_is_a_press() {
        assert_eq!(dragged(100.0, 102.0), Dragged::Click);
        assert_eq!(dragged(100.0, 100.0), Dragged::Click);
        assert_eq!(dragged(100.0, 140.0), Dragged::Zoom);
        // Backwards counts the same distance.
        assert_eq!(dragged(140.0, 100.0), Dragged::Zoom);
        assert_eq!(dragged(102.0, 100.0), Dragged::Click);
    }

    // ------------------------------------------ what the pipeline diagram reads ---

    /// `pipeline3` and a real dump of it, opened the way the window does.
    fn opened() -> WaveState {
        let path = rtlscope_fixtures::path("pipeline3.sv");
        let (uir, _) = rtlscope_sv::lower_files(&[path], &rtlscope_sv::ParseOptions::default());
        let design = rtlscope_elab::elaborate(&uir, Some("pipeline3")).0.expect("elaborates");
        let flat = rtlscope_analyse::flat::flatten(&design);
        WaveState::open(rtlscope_fixtures::wave("pipeline3.vcd"), Some((&design, &flat, &[])))
            .expect("the fixture opens")
    }

    /// A dump of `fsm_enum` written by hand.
    ///
    /// Hand-written rather than simulated: what is under test is the naming,
    /// and four state changes say as much about that as four thousand would.
    const STATES: &str = "$timescale 1ns $end
$scope module tb $end
$scope module dut $end
$var wire 1 aa clk $end
$var wire 1 ab rst_n $end
$var wire 1 ac start $end
$var wire 1 ad busy $end
$var wire 2 ae state $end
$var wire 2 af next_state $end
$upscope $end
$upscope $end
$enddefinitions $end
#0
0aa
0ab
0ac
0ad
b00 ae
b00 af
#10
b01 ae
1ad
#20
b10 ae
#30
b00 ae
0ad
";

    /// A panel showing `fsm_enum`'s `state`, with that dump behind it.
    fn state_machine() -> WaveState {
        // A directory of its own per call. These tests run in parallel and
        // shared one file: a truncating write while another was reading it
        // failed only in the whole suite, never on its own.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let nth = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rtlscope-enum-names-{nth}"));
        let _ = std::fs::create_dir_all(&dir);
        let dump = dir.join("fsm_enum.vcd");
        std::fs::write(&dump, STATES).expect("writes");

        let source = rtlscope_fixtures::path("fsm_enum.sv");
        let (uir, _) = rtlscope_sv::lower_files(&[source], &rtlscope_sv::ParseOptions::default());
        let design = rtlscope_elab::elaborate(&uir, Some("fsm_enum")).0.expect("elaborates");
        let flat = rtlscope_analyse::flat::flatten(&design);
        let mut state = WaveState::open(dump, Some((&design, &flat, &[]))).expect("the dump opens");
        // `open` lays out what it recognises; the enum signals are not
        // among them, so this asks for the one under test by name.
        assert!(state.add_by_dump_path("tb.dut.state").shown(), "{}", state.status);
        state
    }

    /// Which row a signal is on. `open` lays the matched signals out itself, so
    /// counting to a row here would be a test of that order rather than of the
    /// naming.
    fn row(state: &WaveState, wanted: &str) -> usize {
        state
            .tracks
            .iter()
            .position(|track| matches!(track, Track::Signal { name, .. } if name == wanted))
            .unwrap_or_else(|| panic!("no row for `{wanted}`"))
    }

    /// Elaboration folds `S_RUN` to `2'd1`, and `1` is not what the author
    /// wrote or what a reader debugging a state machine is looking for.
    #[test]
    fn a_state_signal_reads_by_the_name_its_enum_gives_it() {
        let mut state = state_machine();
        let state_row = row(&state, "tb.dut.state");

        state.cursor = Some(15);
        assert_eq!(state.value_of(Layer::Recorded, state_row).as_deref(), Some("S_RUN"));

        state.cursor = Some(25);
        assert_eq!(state.value_of(Layer::Recorded, state_row).as_deref(), Some("S_DONE"));

        state.cursor = Some(5);
        assert_eq!(state.value_of(Layer::Recorded, state_row).as_deref(), Some("S_IDLE"));
    }

    /// A signal the design does not declare gets no names, and a signal that
    /// is not an enum keeps its number. Naming everything that happened to
    /// equal an enum member would rename half the panel after a type it has
    /// nothing to do with.
    #[test]
    fn a_signal_without_an_enum_type_still_reads_as_a_number() {
        let mut state = state_machine();
        let busy = row(&state, "tb.dut.busy");
        state.cursor = Some(15);
        assert_eq!(state.value_of(Layer::Recorded, busy).as_deref(), Some("1"));
    }

    /// The names hold for a recording a simulator made, of a register inside
    /// an instance, whose enum is declared in that instance's module — the
    /// shape of every real design, rather than of a hand-written dump of a top.
    #[test]
    fn a_state_inside_an_instance_reads_by_its_enum_names_from_a_real_recording() {
        let source = rtlscope_fixtures::path("axi4_demo.sv");
        let (uir, _) = rtlscope_sv::lower_files(&[source], &rtlscope_sv::ParseOptions::default());
        let design = rtlscope_elab::elaborate(&uir, Some("axi4_demo")).0.expect("elaborates");
        let flat = rtlscope_analyse::flat::flatten(&design);
        let mut state =
            WaveState::open(rtlscope_fixtures::wave("axi4_demo.fst"), Some((&design, &flat, &[])))
                .expect("the recording opens");
        assert!(state.add_by_ir_name("u_master.state").shown(), "{}", state.status);
        assert!(state.add_by_ir_name("u_slave.wstate").shown(), "{}", state.status);
        let master = row(&state, "tb_axi4_demo.u_dut.u_master.state");
        let slave = row(&state, "tb_axi4_demo.u_dut.u_slave.wstate");

        // Resting after reset; then the four data beats; then the read.
        state.cursor = Some(50_000);
        assert_eq!(state.value_of(Layer::Recorded, master).as_deref(), Some("M_IDLE"));
        state.cursor = Some(150_000);
        assert_eq!(state.value_of(Layer::Recorded, master).as_deref(), Some("M_W"));
        assert_eq!(state.value_of(Layer::Recorded, slave).as_deref(), Some("W_DATA"));
        state.cursor = Some(250_000);
        assert_eq!(state.value_of(Layer::Recorded, master).as_deref(), Some("M_R"));
        assert_eq!(state.value_of(Layer::Recorded, slave).as_deref(), Some("W_IDLE"));
    }

    /// Two members sharing a value means the name is a matter of taste rather
    /// than of fact, and picking one would be inventing something.
    #[test]
    fn a_value_two_enum_members_share_stays_a_number() {
        let dir = std::env::temp_dir().join("rtlscope-enum-shared");
        let _ = std::fs::create_dir_all(&dir);
        let source = dir.join("shared.sv");
        std::fs::write(
            &source,
            "module shared (input logic clk);\n\
             typedef enum logic [1:0] { A = 2\'d1, B = 2\'d1, C = 2\'d2 } shared_e;\n\
             shared_e state;\n\
             always_ff @(posedge clk) state <= C;\n\
             endmodule\n",
        )
        .expect("writes");

        let (uir, _) = rtlscope_sv::lower_files(&[source], &rtlscope_sv::ParseOptions::default());
        let design = rtlscope_elab::elaborate(&uir, Some("shared")).0.expect("elaborates");
        let flat = rtlscope_analyse::flat::flatten(&design);
        let signal = flat
            .all_names(&design)
            .find(|(name, ..)| name.ends_with("state"))
            .map(|(_, signal, ..)| signal)
            .expect("the design has a `state`");

        let names = names_of(&design, &flat, &[], signal).expect("the enum is found");
        assert_eq!(names.get(&2).map(String::as_str), Some("C"), "one name, one value");
        assert!(!names.contains_key(&1), "two names for 1, so neither is offered: {names:?}");
    }

    /// Reads a fixture and finds one of its signals by name.
    ///
    /// The top's own nets are flattened under no instance at all, so the name
    /// is bare: `state`, not `dut.state`.
    fn design_and_signal(
        fixture: &str,
        top: &str,
        wanted: &str,
    ) -> (rtlscope_ir::Design, Flattened, SignalId) {
        let path = rtlscope_fixtures::path(fixture);
        let (uir, _) = rtlscope_sv::lower_files(&[path], &rtlscope_sv::ParseOptions::default());
        let design = rtlscope_elab::elaborate(&uir, Some(top)).0.expect("elaborates");
        let flat = rtlscope_analyse::flat::flatten(&design);
        let signal = flat
            .all_names(&design)
            .find(|(name, ..)| name == wanted)
            .map(|(_, signal, ..)| signal)
            .unwrap_or_else(|| panic!("{fixture} has no signal called `{wanted}`"));
        (design, flat, signal)
    }

    /// A state register's values are named by the `case` that decides them.
    ///
    /// The layer under the enum, and the reason a row of `fsm.sv` can be spelt
    /// at all: it declares its states as `localparam` and gives the register no
    /// type, so the enum layer has nothing to say about it. Every other
    /// `localparam` is still refused — what makes these different is that the
    /// analysis proved a `case` on this register decides its own next value.
    #[test]
    fn a_state_register_is_named_by_its_case() {
        let (design, flat, signal) = design_and_signal("fsm.sv", "fsm", "state");
        let fsms = rtlscope_analyse::fsm::find(&design);
        assert!(!fsms.is_empty(), "the fixture has a machine");

        assert!(
            names_of(&design, &flat, &[], signal).is_none(),
            "and without the machines there is nothing to name it by"
        );
        let names = names_of(&design, &flat, &fsms, signal).expect("the case names them");
        assert_eq!(names.get(&0).map(String::as_str), Some("S_IDLE"));
        assert_eq!(names.get(&1).map(String::as_str), Some("S_RUN"));
        assert_eq!(names.get(&2).map(String::as_str), Some("S_WAIT"));
    }

    /// And so does the net feeding it, which holds the same encoding one clock
    /// early. Spelling one and numbering the other would leave the reader
    /// translating between two rows of the same thing.
    #[test]
    fn the_next_state_net_wears_the_same_names() {
        let (design, flat, signal) = design_and_signal("fsm.sv", "fsm", "next_state");
        let fsms = rtlscope_analyse::fsm::find(&design);

        let names = names_of(&design, &flat, &fsms, signal).expect("the case names these too");
        assert_eq!(names.get(&1).map(String::as_str), Some("S_RUN"));
    }

    /// A type is a stronger claim than an inference, so a machine whose states
    /// are an `enum` is read the way it was written.
    #[test]
    fn an_enum_typed_state_keeps_its_enum_names() {
        let (design, flat, signal) = design_and_signal("fsm_enum.sv", "fsm_enum", "state");
        let fsms = rtlscope_analyse::fsm::find(&design);

        let enum_only = names_of(&design, &flat, &[], signal).expect("the enum names it");
        let both = names_of(&design, &flat, &fsms, signal).expect("and still does");
        assert_eq!(enum_only, both, "the machines change nothing here");
    }

    /// No value ever carries two names, which is the rule the enum layer
    /// follows and the price of being allowed to sit beside it.
    ///
    /// Measured while writing this: `fsm::find` already refuses to choose. Fed
    /// two `localparam`s with one value it returns a single state, named after
    /// the literal — `2'd1` — rather than after either of them. So the guard in
    /// `named_values` cannot be reached from here, and that is the point of
    /// checking the invariant rather than the branch: if the analysis ever
    /// starts handing up both names, the panel must still refuse them, and this
    /// says so without depending on which layer does the refusing.
    #[test]
    fn a_value_two_states_share_stays_a_number() {
        let dir = std::env::temp_dir().join("rtlscope-state-shared");
        let _ = std::fs::create_dir_all(&dir);
        let source = dir.join("dup.sv");
        std::fs::write(
            &source,
            "module dup (input logic clk, input logic go);\n\
             localparam logic [1:0] A = 2'd0;\n\
             localparam logic [1:0] B = 2'd1;\n\
             localparam logic [1:0] C = 2'd1;\n\
             logic [1:0] state, next_state;\n\
             always_ff @(posedge clk) state <= next_state;\n\
             always_comb begin\n\
             next_state = state;\n\
             case (state)\n\
             A: if (go) next_state = B;\n\
             B: next_state = C;\n\
             C: next_state = A;\n\
             endcase\n\
             end\n\
             endmodule\n",
        )
        .expect("writes");

        let (uir, _) = rtlscope_sv::lower_files(&[source], &rtlscope_sv::ParseOptions::default());
        let design = rtlscope_elab::elaborate(&uir, Some("dup")).0.expect("elaborates");
        let fsms = rtlscope_analyse::fsm::find(&design);
        let fsm = fsms.first().expect("a machine");

        let mut how_many: HashMap<i64, usize> = HashMap::new();
        for state in &fsm.states {
            *how_many.entry(state.value).or_default() += 1;
        }

        let names = named_values(fsm);
        for (value, count) in how_many {
            match count {
                1 => assert!(names.contains_key(&value), "one state holds {value}: {names:?}"),
                _ => assert!(!names.contains_key(&value), "{count} states hold {value}: {names:?}"),
            }
        }
    }

    /// The find box takes a state name, which is the way a reader thinks about
    /// a state machine. It has to be resolved before `Wanted::parse` sees it:
    /// `parse` reads anything holding an `x` as a bit pattern, which is right
    /// for `0bxx` and wrong for a member called `IDLE_TX`.
    #[test]
    fn a_search_takes_a_state_name_where_the_signal_has_one() {
        let state = state_machine();
        let (state_row, busy) = (row(&state, "tb.dut.state"), row(&state, "tb.dut.busy"));

        assert_eq!(state.wanted_for(state_row, "S_RUN"), Some(Wanted::Number(1)));
        assert_eq!(state.wanted_for(state_row, "S_DONE"), Some(Wanted::Number(2)));
        // Still a number and still a pattern: the names are offered first, not
        // instead.
        assert_eq!(state.wanted_for(state_row, "0x2"), Some(Wanted::Number(2)));
        assert_eq!(state.wanted_for(state_row, "x1"), Some(Wanted::Bits("x1".into())));
        assert_eq!(state.wanted_for(state_row, "S_NOWHERE"), None);

        // And a row with no names behaves as it always did.
        assert_eq!(state.wanted_for(busy, "S_RUN"), None);
        assert_eq!(state.wanted_for(busy, "x1"), Some(Wanted::Bits("x1".into())));
    }

    /// The readout beside a row goes red when the two recordings part, and the
    /// comparison is on the string. Two spellings of one value would report a
    /// difference where there is none.
    #[test]
    fn the_reference_readout_wears_the_same_name_as_the_recording() {
        let mut state = state_machine();
        let state_row = row(&state, "tb.dut.state");
        let same = state.path.clone();
        state.open_reference(same).expect("a dump can be held against itself");
        state.cursor = Some(15);

        let recorded = state.value_of(Layer::Recorded, state_row);
        let reference = state.value_of(Layer::Reference, state_row);
        assert_eq!(recorded.as_deref(), Some("S_RUN"));
        assert_eq!(recorded, reference, "the same value, spelled the same way");
    }

    fn domain(state: &WaveState) -> rtlscope_analyse::pipeline::DomainDepth {
        let path = rtlscope_fixtures::path("pipeline3.sv");
        let (uir, _) = rtlscope_sv::lower_files(&[path], &rtlscope_sv::ParseOptions::default());
        let design = rtlscope_elab::elaborate(&uir, Some("pipeline3")).0.expect("elaborates");
        let _ = state;
        rtlscope_analyse::pipeline::analyse(&design).domains.into_iter().next().expect("a domain")
    }

    /// The bridge the pipeline diagram colours itself from: the cursor lands on
    /// a cycle, and every stage says what it held there.
    #[test]
    fn the_cursor_says_what_each_stage_was_holding() {
        let mut state = opened();
        state.open_stages(domain(&state));

        // The fixture is a four-on four-off burst, so cycle 0 has the first
        // beat in stage 0 and nothing yet in the two behind it.
        let at = state.cycles.as_ref().expect("stages are open").at(0).expect("cycle 0");
        state.cursor = Some(at);

        let (clock, cycle, cells) =
            state.stage_cells_at_cursor().expect("the cursor is on a built cycle");
        assert_eq!(clock, "clk");
        assert_eq!(cycle, 0);
        assert_eq!(cells.len(), 3, "one per stage: {cells:?}");
        assert_eq!(cells[0], (0, StageCell::Busy));
        assert_eq!(cells[1], (1, StageCell::Idle));
        assert_eq!(cells[2], (2, StageCell::Idle));

        // Four cycles later the burst has reached the bottom of the pipe.
        let at = state.cycles.as_ref().unwrap().at(4).expect("cycle 4");
        state.cursor = Some(at);
        let (_, cycle, cells) = state.stage_cells_at_cursor().expect("still inside the window");
        assert_eq!(cycle, 4);
        assert_eq!(cells[2], (2, StageCell::Busy), "{cells:?}");
    }

    /// Colour is a claim about a cycle that was read. Without the stages laid
    /// out, or without a cursor, there is no such cycle — and saying nothing is
    /// the only honest answer.
    #[test]
    fn nothing_is_claimed_without_a_cursor_on_a_laid_out_cycle() {
        let mut state = opened();
        state.cursor = Some(100);
        assert!(state.stage_cells_at_cursor().is_none(), "the stages are not laid out yet");

        state.open_stages(domain(&state));
        state.cursor = None;
        assert!(state.stage_cells_at_cursor().is_none(), "there is no cursor");

        // A cursor before the first clock edge is not on a cycle at all.
        state.cursor = Some(0);
        assert!(state.stage_cells_at_cursor().is_none(), "cycle 0 begins at the first edge");
    }
}
