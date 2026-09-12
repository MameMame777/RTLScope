//! The desk: which view is where, and the moves that put one somewhere.
//!
//! Every view is a tab in an [`DockState`], and the arrangement belongs to the
//! reader: a tab dragged onto another group joins it, dropped on an edge splits
//! it, pulled out of the dock becomes a window. This window used to have four
//! fixed places and a note saying nothing ever moves; what that cost was a
//! waveform that could only be read by squeezing the diagram, and a hierarchy
//! nobody could put away. Which view wants the space is a question only the
//! person reading the design can answer.
//!
//! Nothing here draws. The application owns the state and the painting; this
//! owns the arrangement, so the rules about where a view belongs sit in one
//! file rather than scattered through the code that paints.
//!
//! Everything takes the dock as an argument rather than reaching for it,
//! because the application has to hand it out while a view is being drawn —
//! see `RtlScopeApp::desk`.

use std::collections::{BTreeMap, BTreeSet};

use egui::{Pos2, Rect, Vec2, pos2, vec2};
use egui_dock::{
    DockState, Node, NodeIndex, NodePath, Split, Surface, SurfaceIndex, TabIndex, TabPath,
};

/// Which view a tab shows.
///
/// `Copy` and `Eq` because the dock stores it by value and finds it by
/// comparison; `Ord` and `Hash` because a set of them is how the application
/// asks what is on screen, and a set that iterates in a different order each
/// frame would make the answer flicker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Tab {
    /// The module tree, which used to be the fixed left panel.
    Hierarchy,
    /// The block diagram, which used to be the fixed centre.
    Diagram,
    Source,
    Wave,
    Diagnostics,
    Fsm,
    Cdc,
    Lint,
    Pipeline,
    /// Where a waveform is drawn rather than read.
    Stim,
    /// Where a signal is asked where it came from.
    Trace,
}

impl Tab {
    /// Every view, in the order the `view` menu lists them: where you are, what
    /// you are looking at, then the reports about it.
    pub(crate) const ALL: [Tab; 11] = [
        Tab::Hierarchy,
        Tab::Diagram,
        Tab::Source,
        Tab::Wave,
        Tab::Diagnostics,
        Tab::Fsm,
        Tab::Cdc,
        Tab::Lint,
        Tab::Pipeline,
        Tab::Stim,
        Tab::Trace,
    ];

    /// What the toolbar's `tools` menu lists: everything that says something
    /// *about* a design, as opposed to showing the design itself. The reports
    /// in their own order, and then the waveform, which is a reading of a run
    /// rather than of the source and so comes last.
    pub(crate) const TOOLS: [Tab; 8] = [
        Tab::Diagnostics,
        Tab::Fsm,
        Tab::Cdc,
        Tab::Lint,
        Tab::Pipeline,
        Tab::Stim,
        Tab::Trace,
        Tab::Wave,
    ];

    /// The views that share one group by default, and the mark by which that
    /// group is recognised later.
    ///
    /// A view opened after the fact — the waveform when a dump arrives, a tab
    /// the reader closed and asked back — goes wherever these are, because that
    /// is the part of the desk they set aside for reading about the design
    /// rather than looking at it. Recognised by content and not by index: the
    /// reader may have moved the whole group, and it is still the group.
    pub(crate) const REPORTS: [Tab; 7] =
        [Tab::Diagnostics, Tab::Fsm, Tab::Cdc, Tab::Lint, Tab::Pipeline, Tab::Stim, Tab::Trace];

    /// The name a saved arrangement refers to it by.
    ///
    /// Written out rather than serialised as a number, because a layout file
    /// outlives the order of this enum: inserting a view would otherwise move
    /// everybody's tabs around.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Tab::Hierarchy => "Hierarchy",
            Tab::Diagram => "Diagram",
            Tab::Source => "Source",
            Tab::Wave => "Wave",
            Tab::Diagnostics => "Diagnostics",
            Tab::Fsm => "FSM",
            Tab::Cdc => "CDC",
            Tab::Lint => "Lint",
            Tab::Pipeline => "Pipeline",
            Tab::Stim => "Stim",
            Tab::Trace => "Trace",
        }
    }

    /// The view a saved arrangement named, if it is still one of ours.
    pub(crate) fn of(key: &str) -> Option<Tab> {
        Tab::ALL
            .into_iter()
            // Case-insensitively, because the two things that name a view by
            // hand — the knob and a hand-edited layout file — are typed by a
            // person, and `wave` meaning nothing while `Wave` works is a trap
            // with no upside.
            .find(|tab| tab.key().eq_ignore_ascii_case(key))
    }
}

/// A move asked for while the dock was out of the application's hands.
///
/// A view's own action can say "show me the source" in the middle of drawing
/// that view, and at that moment the dock has been taken out so `DockArea` can
/// borrow it. Rather than making every such call site care, the move is written
/// down and made the instant the dock is back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockRequest {
    Show(Tab),
    Detach(Tab),
}

/// How much of the height the diagram and the hierarchy take, and how much of
/// the width goes to each side of the two splits.
///
/// Named because they are used twice: once to build the default arrangement,
/// and once more when a view that was closed has to be given somewhere to come
/// back to, which should land it where it started.
const DIAGRAM_SHARE: f32 = 0.62;
const HIERARCHY_SHARE: f32 = 0.2;
const REPORTS_SHARE: f32 = 0.58;

/// The arrangement a reader who has never moved anything sees.
///
/// The same four places the window used to have, because they were a reasonable
/// answer — the point of the change is that they are no longer the *only*
/// answer. The waveform is deliberately absent: it appears when a recording is
/// opened, which is the only time it has anything to say.
///
/// Built with `split_below` and `split_right` alone. `fraction` is the share of
/// the *left or top* child, and for `Split::Left` and `Split::Above` the new
/// node is that child — so for those two the documented meaning ("how much the
/// old node will occupy") is the opposite of what the code does. Sticking to
/// the two directions where the two agree keeps this readable.
pub(crate) fn default_layout() -> DockState<Tab> {
    let mut dock = DockState::new(vec![Tab::Hierarchy]);
    let tree = dock.main_surface_mut();
    let [top, bottom] = tree.split_below(NodeIndex::root(), DIAGRAM_SHARE, Tab::REPORTS.to_vec());
    let [_hierarchy, _diagram] = tree.split_right(top, HIERARCHY_SHARE, vec![Tab::Diagram]);
    let [reports, _source] = tree.split_right(bottom, REPORTS_SHARE, vec![Tab::Source]);
    // The reports, so the first thing that draws is what the diagnostics say —
    // the view this window opened on before it had a dock.
    tree.set_focused_node(reports);
    dock
}

/// Every view that is actually on screen: the active tab of each group.
///
/// A tab behind another tab is in the dock but not in view, and the difference
/// is the whole of what "is the source where I can see it" means.
pub(crate) fn visible(dock: &DockState<Tab>) -> BTreeSet<Tab> {
    dock.iter_leaves().filter_map(|(_, leaf)| leaf.tabs.get(leaf.active.0).copied()).collect()
}

/// Whether this view is on screen right now.
pub(crate) fn is_visible(dock: &DockState<Tab>, tab: Tab) -> bool {
    dock.iter_leaves().any(|(_, leaf)| leaf.tabs.get(leaf.active.0) == Some(&tab))
}

/// Whether the desk holds this view at all, in view or behind another tab.
pub(crate) fn has(dock: &DockState<Tab>, tab: Tab) -> bool {
    dock.find_tab(&tab).is_some()
}

/// The view in the group the reader last touched, if any.
pub(crate) fn focused(dock: &DockState<Tab>) -> Option<Tab> {
    let path = dock.focused_leaf()?;
    let leaf = dock.leaf(path).ok()?;
    leaf.tabs.get(leaf.active.0).copied()
}

/// Puts a view where the reader can see it.
///
/// Brings it to the front of whatever group it is in and focuses that group; if
/// it is not on the desk at all — closed, or never opened — it is given a place
/// first. This is what every "go to the waveform", "show me that line" and
/// "open the Trace view" in the application comes down to, so that they cannot
/// disagree about what showing a view means.
pub(crate) fn show(dock: &mut DockState<Tab>, tab: Tab) {
    let path = match dock.find_tab(&tab) {
        Some(path) => path,
        None => place(dock, tab),
    };
    // Cannot fail for a path just found or just made, and a layout is never
    // worth an error: the worst case is a view that does not come forward.
    let _ = dock.set_active_tab(path);
    dock.set_focused_node_and_surface(path.node_path());
}

/// Where a view that is not on the desk should appear.
///
/// The structural two go back beside what they belong to, and everything else
/// joins the reports. A view the reader closed and asked back for should turn
/// up where they would look for it, which is where it was.
fn place(dock: &mut DockState<Tab>, tab: Tab) -> TabPath {
    // A main surface with nothing on it cannot be split — `Tree::split` asserts
    // that what it splits is a leaf or a parent — and pushing is the only thing
    // that makes a first leaf.
    if bare(dock) {
        return push(dock, tab);
    }
    match tab {
        Tab::Hierarchy => match dock.find_tab(&Tab::Diagram) {
            // The new node is the left child here, so the fraction is its own
            // share and not the diagram's.
            Some(at) => split(dock, at.node_path(), Split::Left, HIERARCHY_SHARE, tab),
            None => push(dock, tab),
        },
        // Likewise above: the fraction is the diagram's own.
        Tab::Diagram => split(dock, NodePath::MAIN_ROOT, Split::Above, DIAGRAM_SHARE, tab),
        _ => match reports(dock) {
            Some(home) => append(dock, home, tab),
            // No reports left anywhere: under the diagram is where they were.
            None => match dock.find_tab(&Tab::Diagram) {
                Some(at) => split(dock, at.node_path(), Split::Below, DIAGRAM_SHARE, tab),
                None => push(dock, tab),
            },
        },
    }
}

/// Gives a view a window of its own, floating inside the main one.
///
/// If it already has one, that window is brought forward instead of a second
/// being made. `main` is the size of the window everything is inside, so a
/// float cannot open larger than what contains it.
pub(crate) fn detach(dock: &mut DockState<Tab>, tab: Tab, main: Option<[f32; 2]>) {
    let (at, size) = (dock.find_tab(&tab), float_size(tab, main));
    let step = floats(dock);
    let surface = match at {
        // Already floating: raise it. Detaching a float would only make a
        // second window holding the same view.
        Some(path) if !path.surface.is_main() => {
            let _ = dock.set_active_tab(path);
            dock.set_focused_node_and_surface(path.node_path());
            return;
        }
        Some(path) => dock.detach_tab(path, Rect::from_min_size(float_at(size, main, step), size)),
        None => dock.add_window(vec![tab]),
    };
    // Said again afterwards because `detach_tab` shrinks what it is given to
    // four fifths, and `add_window` is told nothing at all. One place decides
    // how big a float opens.
    if let Some(state) = dock.get_window_state_mut(surface) {
        state.set_position(float_at(size, main, step));
        state.set_size(size);
    }
    dock.set_focused_node_and_surface(NodePath::new(surface, NodeIndex::root()));
}

/// How big a float opens: wide for the waveform, which is short of time, and
/// squarer for the reports, which are read down the page.
fn float_size(tab: Tab, main: Option<[f32; 2]>) -> Vec2 {
    let want = match tab {
        Tab::Wave => vec2(1440.0, 660.0),
        _ => vec2(1100.0, 520.0),
    };
    match main {
        Some([w, h]) => want.min(vec2(w, h) * 0.9),
        None => want,
    }
}

/// Where it opens: the middle of the window it floats in, stepped down and
/// across by however many floats are already out.
///
/// Centred so it is never half-off the side of what contains it, and stepped
/// so it is never exactly on top of the last one. Two views ejected in a row —
/// or asked for together, which `RTLSCOPE_POPOUT=Wave,Trace` does — landed on the
/// same spot and read as one window that had opened the wrong view.
fn float_at(size: Vec2, main: Option<[f32; 2]>, out: usize) -> Pos2 {
    // Enough to see the edge and the tab beneath, and wrapped so a reader who
    // ejects six things does not send the sixth off the bottom.
    let step = (out % 6) as f32 * 28.0;
    match main {
        Some([w, h]) => {
            let middle = pos2(((w - size.x) / 2.0).max(0.0), ((h - size.y) / 2.0).max(0.0));
            // Never past the far edge: the step gives way to the window.
            pos2(
                (middle.x + step).min((w - size.x).max(0.0)),
                (middle.y + step).min((h - size.y).max(0.0)),
            )
        }
        None => pos2(48.0 + step, 64.0 + step),
    }
}

/// How many views are already in windows of their own.
fn floats(dock: &DockState<Tab>) -> usize {
    dock.iter_surfaces().filter(|surface| matches!(surface, Surface::Window(..))).count()
}

/// The group on the main surface that holds the reports, if there still is one.
fn reports(dock: &DockState<Tab>) -> Option<NodePath> {
    dock.iter_leaves()
        .find(|(path, leaf)| {
            path.surface.is_main() && leaf.tabs.iter().any(|tab| Tab::REPORTS.contains(tab))
        })
        .map(|(path, _)| path)
}

/// Whether the main surface has nothing on it.
fn bare(dock: &DockState<Tab>) -> bool {
    match dock.get_surface(SurfaceIndex::main()) {
        Some(Surface::Main(tree)) => tree.num_tabs() == 0,
        _ => true,
    }
}

fn split(dock: &mut DockState<Tab>, at: NodePath, how: Split, fraction: f32, tab: Tab) -> TabPath {
    let [_old, new] = dock.split(at, how, fraction, Node::leaf(tab));
    TabPath::new(at.surface, new, TabIndex(0))
}

fn append(dock: &mut DockState<Tab>, at: NodePath, tab: Tab) -> TabPath {
    dock[at].append_tab(tab);
    TabPath::new(at.surface, at.node, TabIndex(dock[at].tabs_count() - 1))
}

fn push(dock: &mut DockState<Tab>, tab: Tab) -> TabPath {
    dock.push_to_first_leaf(tab);
    dock.find_tab(&tab).expect("push_to_first_leaf always lands somewhere")
}

/// Where each floating window is, right now.
///
/// Asked of egui rather than of the dock, because the dock does not know. It
/// keeps a `screen_rect` for every window and, in this version, never writes to
/// it — `WindowState::rect` answers `Rect::NOTHING` for the life of the
/// program. The window itself is an `egui::Window` under an id the dock builds
/// from the surface's number, and egui does remember where one of those has
/// been dragged to, so that is who to ask.
///
/// Read every frame rather than on the way out, because a process killed by the
/// session ending never gets to say goodbye.
pub(crate) fn float_places(dock: &DockState<Tab>, ctx: &egui::Context) -> BTreeMap<usize, Rect> {
    dock.iter_surfaces_indexed()
        .filter(|(_, surface)| matches!(surface, Surface::Window(..)))
        .filter_map(|(index, _)| {
            let at = ctx.memory(|memory| memory.area_rect(window_id(index)))?;
            // Believable only: a window that has not been drawn yet has no
            // size, and writing that down would reopen at nothing.
            let sane = at.width() > 40.0 && at.height() > 40.0 && at.is_finite();
            sane.then_some((index.0, at))
        })
        .collect()
}

/// The name egui knows a floating window's area by.
///
/// The dock builds it as `format!("window {surface:?}")` and hands it to
/// `egui::Window::id`. Spelled out here rather than taken from anywhere,
/// because it is not exposed — so it is checked by a test, and a version of the
/// dock that renames its windows costs the float positions and nothing worse.
fn window_id(surface: SurfaceIndex) -> egui::Id {
    egui::Id::new(format!("window {surface:?}"))
}

/// The desk, as names, ready to be written down.
///
/// By name rather than by number, for the same reason [`Tab::key`] exists: a
/// file written today has to be readable by a build that has since gained a
/// view.
///
/// `places` is where the floating windows were last seen. They travel in the
/// same field the dock uses to place a window it is about to draw for the first
/// time, so restoring one needs no code at all: the first frame takes the
/// position out and puts the window there.
pub(crate) fn to_saved(
    dock: &DockState<Tab>,
    places: &BTreeMap<usize, Rect>,
) -> Option<serde_json::Value> {
    let mut named = dock.map_tabs(|tab| tab.key().to_string());
    forget_rects(&mut named);
    for (index, at) in places {
        if let Some(state) = named.get_window_state_mut(SurfaceIndex(*index)) {
            state.set_position(at.min);
            state.set_size(at.size());
        }
    }
    serde_json::to_value(named).ok()
}

/// Wipes the rectangles the dock keeps for a node before it is written down.
///
/// They are this frame's arithmetic — where each pane landed on this screen —
/// and are worked out again from the fractions the moment anything is drawn, so
/// nothing is lost. What is gained is a file that can be read back at all: an
/// undrawn node's rectangle is `Rect::NOTHING`, whose corners are infinities,
/// and JSON has no way to write one. `serde_json` turns them into `null`
/// silently on the way out and then refuses them on the way in, which cost a
/// saved arrangement its whole trip and said nothing about why.
fn forget_rects(dock: &mut DockState<String>) {
    for (_, node) in dock.iter_all_nodes_mut() {
        node.set_rect(Rect::ZERO);
        if let Some(leaf) = node.get_leaf_mut() {
            leaf.viewport = Rect::ZERO;
        }
    }
}

/// The desk a file described, as far as it still makes sense.
///
/// A name this build no longer knows costs that one tab and nothing else —
/// which is the promise the layout file makes. `None` means the file described
/// nothing usable, and the caller should keep the default arrangement.
pub(crate) fn from_saved(value: serde_json::Value) -> Option<DockState<Tab>> {
    let named: DockState<String> = serde_json::from_value(value).ok()?;
    let mut dock = named.filter_map_tabs(|name| Tab::of(name));

    // Windows that lost their last view, closed by hand. `filter_map_tabs`
    // takes the tabs out but leaves the surface standing — its tree keeps an
    // empty node, so it does not read as empty — and a surface that is still
    // there is still drawn. Left alone, a file naming one view we no longer
    // have opened an empty window floating over the desk with nothing in it and
    // no way to tell what it was for.
    let spent: Vec<SurfaceIndex> = dock
        .iter_surfaces_indexed()
        .filter(|(index, surface)| {
            !index.is_main() && surface.node_tree().is_none_or(|tree| tree.num_tabs() == 0)
        })
        .map(|(index, _)| index)
        .collect();
    for index in spent {
        dock.remove_surface(index);
    }

    // Nothing left on the main surface is not an arrangement worth restoring:
    // it would open a window whose whole contents are floating over an empty
    // hole. Rebuilding "the default, plus their floats" is more cleverness than
    // a state only a hand-edited file can reach deserves.
    if bare(&dock) {
        return None;
    }

    // Focus, put back by hand. Dropping a name can empty a floating window,
    // and an emptied window is removed and the ones after it renumbered — while
    // the surface that was in focus is copied across untouched. Left alone that
    // is an index pointing past the end of the list, and the first frame that
    // indexes it panics. Measured on a file naming a view we no longer have.
    let focus = dock
        .find_tab(&Tab::Diagram)
        .map(|path| path.node_path())
        .or_else(|| dock.iter_leaves().next().map(|(path, _)| path))?;
    dock.set_focused_node_and_surface(focus);
    Some(dock)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A desk named by hand, written the way a real one is written.
    ///
    /// Through the same rect-wiping `to_saved` does, because an undrawn node's
    /// rectangle is infinite and JSON cannot carry one — a test that skipped
    /// this would be checking `from_saved` against a file no run could produce.
    fn written(mut named: DockState<String>) -> serde_json::Value {
        forget_rects(&mut named);
        serde_json::to_value(named).expect("writes")
    }

    /// The names in a layout file are a contract with every file already
    /// written. A key that two views share, or one that does not read back,
    /// would silently rearrange somebody's desk.
    #[test]
    fn every_view_survives_the_name_a_layout_calls_it_by() {
        let mut seen = BTreeSet::new();
        for tab in Tab::ALL {
            let key = tab.key();
            assert!(seen.insert(key), "two views answer to {key}");
            assert_eq!(Tab::of(key), Some(tab), "{key} does not read back");
            assert_eq!(Tab::of(&key.to_lowercase()), Some(tab), "{key} is case-bound");
        }
        assert_eq!(Tab::of("Waveform"), None, "a name we never wrote is not a view");
        assert_eq!(Tab::of("Diagram"), Some(Tab::Diagram), "the diagram is a view now");
        assert_eq!(Tab::of("hierarchy"), Some(Tab::Hierarchy), "and so is the tree");
    }

    /// The arrangement a first run sees. Every view once, in the four places
    /// the window has always had them.
    #[test]
    fn the_default_desk_has_every_view_but_the_waveform() {
        let dock = default_layout();
        for tab in Tab::ALL {
            let found = dock.iter_all_tabs().filter(|(_, held)| **held == tab).count();
            match tab {
                // Nothing to show until a recording is opened.
                Tab::Wave => assert_eq!(found, 0, "the waveform waits for a dump"),
                _ => assert_eq!(found, 1, "{} is on the desk exactly once", tab.key()),
            }
        }
        assert!(
            dock.iter_all_tabs().all(|(path, _)| path.surface.is_main()),
            "and nothing starts in a window of its own"
        );
        assert_eq!(dock.iter_leaves().count(), 4, "four places, as before");
        assert_eq!(
            visible(&dock),
            BTreeSet::from([Tab::Hierarchy, Tab::Diagram, Tab::Diagnostics, Tab::Source]),
        );
    }

    /// The source is beside the reports, not behind them: reading a state
    /// machine and the `case` it came from together is the arrangement the
    /// window was rebuilt around.
    #[test]
    fn the_source_has_a_place_of_its_own_beside_the_reports() {
        let dock = default_layout();
        let source = dock.find_tab(&Tab::Source).expect("on the desk");
        let reports = dock.find_tab(&Tab::Diagnostics).expect("likewise");
        assert_ne!(source.node, reports.node, "not behind the reports");
        for tab in Tab::REPORTS {
            let path = dock.find_tab(&tab).expect("on the desk");
            assert_eq!(path.node, reports.node, "{} shares the reports' group", tab.key());
        }
        let hierarchy = dock.find_tab(&Tab::Hierarchy).expect("on the desk");
        let diagram = dock.find_tab(&Tab::Diagram).expect("likewise");
        assert_ne!(hierarchy.node, diagram.node, "the tree is beside the diagram");
    }

    /// A dump arriving puts the waveform where the other reading is done.
    #[test]
    fn a_view_that_was_not_there_joins_the_reports() {
        let mut dock = default_layout();
        show(&mut dock, Tab::Wave);

        let wave = dock.find_tab(&Tab::Wave).expect("now on the desk");
        let reports = dock.find_tab(&Tab::Diagnostics).expect("still there");
        assert_eq!(wave.node, reports.node, "in with the reports");
        assert!(is_visible(&dock, Tab::Wave), "and in front of them");
        assert!(!is_visible(&dock, Tab::Diagnostics), "which are now behind it");
    }

    /// Showing a view also moves the focus to it, so the next thing pushed to
    /// the focused group lands where the reader is looking.
    #[test]
    fn showing_a_view_focuses_where_it_is() {
        let mut dock = default_layout();
        show(&mut dock, Tab::Source);
        assert_eq!(
            dock.focused_leaf(),
            Some(dock.find_tab(&Tab::Source).expect("on the desk").node_path()),
        );
    }

    /// A closed view comes back where it was, not wherever there was room.
    #[test]
    fn a_closed_view_comes_back_where_it_belongs() {
        let mut dock = default_layout();

        let lint = dock.find_tab(&Tab::Lint).expect("on the desk");
        dock.remove_tab(lint);
        assert!(!has(&dock, Tab::Lint), "gone");
        show(&mut dock, Tab::Lint);
        let back = dock.find_tab(&Tab::Lint).expect("and back");
        let reports = dock.find_tab(&Tab::Diagnostics).expect("still there");
        assert_eq!(back.node, reports.node, "with the reports it was among");

        let hierarchy = dock.find_tab(&Tab::Hierarchy).expect("on the desk");
        dock.remove_tab(hierarchy);
        show(&mut dock, Tab::Hierarchy);
        let back = dock.find_tab(&Tab::Hierarchy).expect("back");
        let diagram = dock.find_tab(&Tab::Diagram).expect("still there");
        assert!(back.surface.is_main(), "on the main surface");
        assert_ne!(back.node, diagram.node, "beside the diagram rather than behind it");
        assert!(is_visible(&dock, Tab::Hierarchy) && is_visible(&dock, Tab::Diagram));
    }

    /// Nothing on the desk at all is a state the reader can reach by closing
    /// every tab, and asking for a view then has to work.
    #[test]
    fn a_view_asked_for_on_an_empty_desk_makes_the_first_group() {
        let mut dock: DockState<Tab> = DockState::new(vec![]);
        assert!(bare(&dock));
        show(&mut dock, Tab::Diagnostics);
        assert!(is_visible(&dock, Tab::Diagnostics));
        show(&mut dock, Tab::Lint);
        assert!(is_visible(&dock, Tab::Lint), "and the next joins it");
    }

    /// A float is a window, not a copy: asking twice raises the one there is.
    #[test]
    fn a_view_given_a_window_leaves_the_main_surface() {
        let mut dock = default_layout();
        detach(&mut dock, Tab::Trace, Some([1400.0, 900.0]));

        let trace = dock.find_tab(&Tab::Trace).expect("still on the desk");
        assert!(!trace.surface.is_main(), "in a window of its own");
        assert!(dock.get_window_state(trace.surface).is_some(), "with a window to be in");
        let windows = dock.iter_all_tabs().filter(|(path, _)| !path.surface.is_main()).count();

        detach(&mut dock, Tab::Trace, Some([1400.0, 900.0]));
        assert_eq!(
            dock.iter_all_tabs().filter(|(path, _)| !path.surface.is_main()).count(),
            windows,
            "asking again raises it rather than making a second window",
        );
    }

    /// The whole desk has to survive the trip to disk and back — that is the
    /// feature.
    #[test]
    fn an_arrangement_comes_back_from_its_own_names() {
        let mut dock = default_layout();
        show(&mut dock, Tab::Wave);
        detach(&mut dock, Tab::Wave, Some([1400.0, 900.0]));

        let saved = to_saved(&dock, &BTreeMap::new()).expect("writes");
        let back = from_saved(saved).expect("and reads");

        for tab in Tab::ALL {
            assert!(has(&back, tab), "{} came back", tab.key());
        }
        let wave = back.find_tab(&Tab::Wave).expect("came back");
        assert!(!wave.surface.is_main(), "still floating");
        assert!(back.focused_leaf().is_some(), "and something has the focus");
    }

    /// A layout is a convenience, and a convenience that complains is worse
    /// than one that quietly does its best.
    #[test]
    fn a_name_we_no_longer_know_costs_only_that_tab() {
        let named = DockState::new(vec!["Lint".to_string(), "NoSuchView".to_string()]);
        let back = from_saved(written(named)).expect("reads");

        assert!(has(&back, Tab::Lint), "the view we still have");
        assert_eq!(back.iter_all_tabs().count(), 1, "and only that one");
    }

    /// Dropping a name can empty a floating window, and the surfaces after it
    /// are renumbered while the focus is not. Left alone that focus points past
    /// the end of the list and the first frame drawn panics.
    #[test]
    fn a_focus_left_pointing_at_a_dropped_window_does_not_survive_as_one() {
        let mut named = DockState::new(vec!["Diagram".to_string()]);
        let window = named.add_window(vec!["NoSuchView".to_string()]);
        named.set_focused_node_and_surface(NodePath::new(window, NodeIndex::root()));

        let back = from_saved(written(named)).expect("reads");

        assert!(
            !back.iter_surfaces().any(|surface| matches!(surface, Surface::Window(tree, _)
                if tree.num_tabs() == 0)),
            "no window is left standing with nothing in it",
        );
        let focus = back.focused_leaf().expect("and the focus went somewhere real");
        assert!(focus.surface.is_main());
        // The panic this guards against is on the very next frame, so ask the
        // same question the drawing code would.
        assert!(back.get_surface(focus.surface).is_some());
    }

    /// A saved desk with nothing left on the main surface would open as a hole
    /// with windows over it.
    #[test]
    fn an_arrangement_with_nothing_left_underneath_is_not_used() {
        let mut named: DockState<String> = DockState::new(vec![]);
        named.add_window(vec!["NoSuchView".to_string()]);
        assert!(from_saved(written(named)).is_none(), "nothing underneath is nothing to restore");
    }

    /// Two floats in a row must not land on the same spot: one exactly behind
    /// the other reads as a single window showing the wrong view. Measured on
    /// `RTLSCOPE_POPOUT=Wave,Trace`, which put both in the middle and looked like
    /// one window that had opened the wrong one.
    #[test]
    fn a_second_float_does_not_open_on_top_of_the_first() {
        let main = Some([1400.0, 900.0]);
        let size = vec2(1100.0, 520.0);
        let places: Vec<Pos2> = (0..3).map(|out| float_at(size, main, out)).collect();
        assert_ne!(places[0], places[1], "{places:?}");
        assert_ne!(places[1], places[2], "{places:?}");
        for at in &places {
            assert!(at.x >= 0.0 && at.y >= 0.0, "never off the top or the left: {at:?}");
            assert!(at.x + size.x <= 1400.0 + 0.01, "nor off the right: {at:?}");
            assert!(at.y + size.y <= 900.0 + 0.01, "nor off the bottom: {at:?}");
        }

        // And the count the step is taken from follows the windows.
        let mut dock = default_layout();
        assert_eq!(floats(&dock), 0);
        detach(&mut dock, Tab::Trace, main);
        assert_eq!(floats(&dock), 1);
        detach(&mut dock, Tab::Lint, main);
        assert_eq!(floats(&dock), 2, "a second window, not a second tab in the first");
    }

    /// The name egui knows a floating window by is built from a format string
    /// inside the dock, which is not exposed and could change under us. Pinned
    /// here so that a version bump which renames it fails a test rather than
    /// quietly costing every float its position.
    #[test]
    fn a_floating_window_is_known_by_the_name_the_dock_gives_it() {
        assert_eq!(window_id(SurfaceIndex(1)), egui::Id::new("window SurfaceIndex(1)"));
        assert_ne!(window_id(SurfaceIndex(1)), window_id(SurfaceIndex(2)));
    }

    /// Where a float was put has to survive the trip to disk, which is the
    /// half of the arrangement the dock does not record for itself.
    #[test]
    fn a_float_comes_back_where_it_was_left() {
        let mut dock = default_layout();
        detach(&mut dock, Tab::Trace, Some([1400.0, 900.0]));
        let surface = dock.find_tab(&Tab::Trace).expect("floating").surface;

        let was = Rect::from_min_size(pos2(210.0, 130.0), vec2(760.0, 430.0));
        let places = BTreeMap::from([(surface.0, was)]);
        let written = to_saved(&dock, &places).expect("writes");

        // Read out of the file rather than off the state: the fields it travels
        // in are the dock's own, and it does not let anyone else look at them.
        let at = &written["surfaces"][surface.0]["Window"][1];
        assert_eq!(at["next_position"]["x"], 210.0, "{at}");
        assert_eq!(at["next_position"]["y"], 130.0, "{at}");
        assert_eq!(at["next_size"]["x"], 760.0, "{at}");
        assert_eq!(at["next_size"]["y"], 430.0, "{at}");

        // And it is still there after the trip, ready for the first frame that
        // draws the window to take it out and put the window there.
        let back = from_saved(written).expect("reads");
        let again = to_saved(&back, &BTreeMap::new()).expect("writes again");
        let trace = back.find_tab(&Tab::Trace).expect("still floating");
        assert!(!trace.surface.is_main());
        assert_eq!(again["surfaces"][trace.surface.0]["Window"][1]["next_position"]["x"], 210.0);
    }

    /// A float opens inside what contains it, whatever it asked for.
    #[test]
    fn a_float_is_never_larger_than_the_window_it_floats_in() {
        let small = float_size(Tab::Wave, Some([1000.0, 700.0]));
        assert!(small.x <= 900.0 && small.y <= 630.0, "clamped to nine tenths: {small:?}");
        let roomy = float_size(Tab::Wave, Some([2560.0, 1400.0]));
        assert_eq!(roomy, vec2(1440.0, 660.0), "and never grown past what it wanted");

        let at = float_at(vec2(1440.0, 660.0), Some([1400.0, 900.0]), 0);
        assert!(at.x >= 0.0 && at.y >= 0.0, "never off the top or the left: {at:?}");
    }
}
