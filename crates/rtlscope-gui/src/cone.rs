//! The cone of influence, drawn.
//!
//! [`rtlscope_analyse::cone`] answers "what decides this" and "what does this
//! decide" as levels of signals. Levels are columns, which is the whole of the
//! layout: there is no placement problem to solve here, unlike the block
//! diagram, because distance from the root *is* the position. That is why this
//! does not go through the layout engine — putting it there would be asking a
//! ranking algorithm to rediscover a number the analysis already knows.
//!
//! The pan and zoom are [`crate::canvas::View`], the same transform the diagram
//! uses, for the same reason: a scaled layer stretches its glyphs, and a
//! picture of nothing but names cannot afford that.
//!
//! Reading direction follows the question. Asking what decides a signal puts
//! the root on the right and its causes to the left, which is how a schematic
//! reads; asking what it decides puts the root on the left and the consequences
//! to the right. The arrows always point the way the value flows.

use std::collections::HashMap;

use egui::{Align2, CornerRadius, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use rtlscope_analyse::cone::{Cone, EdgeKind, Towards};
use rtlscope_analyse::flat::{Flattened, SignalId};

use crate::canvas::{LEGIBLE, Placement, ZOOM_MAX};
use crate::theme::Theme;

/// How far apart the columns are, in sheet units.
const COLUMN: f64 = 240.0;
/// How far apart the rows are.
const ROW: f64 = 36.0;
/// A box's height. The width comes from the name it holds.
const BOX_HEIGHT: f64 = 24.0;

/// What the reader did to the cone.
#[derive(Debug, Default)]
pub struct ConeAction {
    /// A signal was clicked: select it, the same as clicking its wire.
    pub picked: Option<SignalId>,
    /// A signal was double-clicked: ask the same question about it instead.
    ///
    /// This is how a cone is walked. One question answered usually produces the
    /// next one, and re-rooting is that next question without going back to the
    /// diagram to find the wire.
    pub rerooted: Option<SignalId>,
}

/// Where each signal's box sits, in sheet units.
struct Placed {
    at: HashMap<SignalId, (f64, f64, f64)>,
    sheet: Rect,
}

/// Columns by level, rows by name.
///
/// Sorted by name inside a column rather than left in the order the walk found
/// them: a picture that reshuffles itself when an unrelated part of the design
/// changes is one nobody trusts.
fn place(cone: &Cone, flat: &Flattened) -> Placed {
    let deepest = cone.depth();
    let mut at = HashMap::new();
    let (mut wide, mut tall) = (0.0f64, 0.0f64);

    for level in 0..=deepest {
        let mut here: Vec<(String, SignalId)> =
            cone.level(level).map(|node| (flat.name_of(node.signal), node.signal)).collect();
        here.sort();

        // Drivers read right to left, loads left to right. The root is the
        // fixed end either way.
        let column = match cone.towards {
            Towards::Drivers => (deepest - level) as f64,
            Towards::Loads => level as f64,
        };
        // Centred against the tallest column, so the root sits opposite the
        // middle of what it faces rather than at the top of it.
        let offset = -(here.len() as f64 - 1.0) * ROW / 2.0;
        for (row, (name, signal)) in here.into_iter().enumerate() {
            let width = (name.chars().count() as f64 * 6.6 + 16.0).max(60.0);
            let x = column * COLUMN;
            let y = offset + row as f64 * ROW;
            at.insert(signal, (x, y, width));
            wide = wide.max(x + width);
            tall = tall.max(y.abs() + BOX_HEIGHT);
        }
    }

    // A floor under the sheet, so a cone of one or two boxes is not blown up
    // to fill the pane. Fitting is right for a picture; for a near-empty one it
    // turns a name into a headline.
    let wide = wide.max(COLUMN * 2.5);
    let tall = tall.max(ROW * 3.0);
    Placed {
        at,
        sheet: Rect::from_min_max(
            Pos2::new(-20.0, -(tall as f32) - 20.0),
            Pos2::new(wide as f32 + 20.0, tall as f32 + 20.0),
        ),
    }
}

/// Draws a cone and reports what was done to it.
pub fn show(
    ui: &mut Ui,
    cone: &Cone,
    flat: &Flattened,
    placement: &mut Placement,
    lit: Option<SignalId>,
) -> ConeAction {
    let theme = Theme::of(ui);
    // Claimed, not merely looked at. A resizable panel remembers the rectangle
    // its *content* occupied, so a view that draws over the space without
    // taking it leaves the panel storing the height of nothing — and the pane
    // springs back to a sliver the moment the reader lets go of the splitter.
    // Measured: the diagnostics pane, whose scroll area fills itself, holds
    // whatever height it is dragged to; this one did not.
    let viewport = ui.available_rect_before_wrap();
    let background = ui.allocate_rect(viewport, Sense::click_and_drag());
    let placed = place(cone, flat);
    // Placed where it was left, at the size it was left. A pane made taller
    // shows more of the same drawing rather than a differently scaled one.
    let mut view = placement.view(viewport, placed.sheet, ZOOM_MAX);
    view.steer(ui, &background);

    let painter = ui.painter_at(viewport);
    painter.rect_filled(viewport, 0.0, theme.canvas_bg);

    // The edges first, so a box is never drawn under a line.
    for (from, to, kind) in &cone.edges {
        let (Some(a), Some(b)) = (placed.at.get(from), placed.at.get(to)) else { continue };
        let (start, end) =
            (view.at(a.0 + a.2, a.1 + BOX_HEIGHT / 2.0), view.at(b.0, b.1 + BOX_HEIGHT / 2.0));
        // Right to left when the drivers read that way, so the line leaves the
        // side of the box the value leaves by.
        let (start, end) = match cone.towards {
            Towards::Drivers => {
                (view.at(a.0, a.1 + BOX_HEIGHT / 2.0), view.at(b.0 + b.2, b.1 + BOX_HEIGHT / 2.0))
            }
            Towards::Loads => (start, end),
        };
        let colour = match kind {
            EdgeKind::Clocked => theme.clock,
            EdgeKind::Comb => theme.wire,
        };
        let mid = (start.x + end.x) / 2.0;
        for pair in [
            [start, Pos2::new(mid, start.y)],
            [Pos2::new(mid, start.y), Pos2::new(mid, end.y)],
            [Pos2::new(mid, end.y), end],
        ] {
            painter.line_segment(pair, Stroke::new(1.2 * view.scale.clamp(0.5, 2.0), colour));
        }
        // A register on the line, where the value waits a clock. Small, because
        // it is a note on the edge and not a thing in its own right.
        if *kind == EdgeKind::Clocked {
            let side = 5.0 * view.scale.clamp(0.5, 2.0);
            painter.rect_filled(
                Rect::from_center_size(Pos2::new(mid, (start.y + end.y) / 2.0), Vec2::splat(side)),
                1.0,
                colour,
            );
        }
    }

    let mut action = ConeAction::default();
    let font = view.font(11.0);

    for node in &cone.nodes {
        let Some((x, y, width)) = placed.at.get(&node.signal).copied() else { continue };
        let rect = view.rect(x, y, width, BOX_HEIGHT);
        if !viewport.intersects(rect) {
            continue;
        }
        let response = ui.interact(rect, ui.id().with(("cone-node", node.signal)), Sense::click());

        let is_root = node.signal == cone.root;
        let (fill, stroke) = if is_root {
            (theme.accent_soft, theme.accent)
        } else if lit == Some(node.signal) {
            (theme.accent_soft, theme.highlight)
        } else if node.frontier {
            (theme.surface_alt, theme.clock)
        } else {
            (theme.box_fill, theme.box_stroke)
        };
        let weight = if is_root || response.hovered() { 2.0 } else { 1.0 };
        painter.rect_filled(rect, CornerRadius::same(3), fill);
        painter.rect_stroke(
            rect,
            CornerRadius::same(3),
            Stroke::new(weight * view.scale.clamp(0.5, 2.0), stroke),
            egui::StrokeKind::Inside,
        );
        if font.size >= LEGIBLE {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                flat.name_of(node.signal),
                font.clone(),
                theme.ink,
            );
        }

        if response.clicked() {
            action.picked = Some(node.signal);
        }
        if response.double_clicked() {
            action.rerooted = Some(node.signal);
        }
        response.on_hover_text(match (is_root, node.frontier) {
            (true, _) => "the signal being asked about".to_string(),
            (false, true) => format!(
                "{} hop(s) away, across a register — double-click to ask about it",
                node.level
            ),
            (false, false) => {
                format!("{} hop(s) away — double-click to ask about it", node.level)
            }
        });
    }

    // What was left out, said on the picture rather than only in a report.
    if cone.clipped > 0 {
        painter.text(
            viewport.left_bottom() + Vec2::new(8.0, -8.0),
            Align2::LEFT_BOTTOM,
            format!("{} more left out — a level that wide is not a picture", cone.clipped),
            egui::FontId::proportional(11.0),
            theme.warn,
        );
    }
    if cone.nodes.len() == 1 {
        painter.text(
            // Along the bottom, clear of whatever the fit put in the middle.
            viewport.center_bottom() - Vec2::new(0.0, 10.0),
            Align2::CENTER_BOTTOM,
            match cone.towards {
                Towards::Drivers => "nothing decides this — it is an input of the design",
                Towards::Loads => "nothing reads this — it is an output, or it is dead",
            },
            egui::FontId::proportional(12.0),
            theme.muted,
        );
    }

    // Kept as what it is — a scale and an offset — rather than as a region to
    // be fitted back in. Nothing has to be recovered, so nothing can drift.
    placement.remember(&view, viewport);
    action
}
