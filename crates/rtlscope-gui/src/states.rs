//! Painting a [`StateGeom`] with `egui`.
//!
//! The same doctrine as [`crate::canvas`]: `rtlscope-graph` decides where
//! everything goes and this only turns that into shapes, so a layout bug is
//! visible in the geometry tests rather than only on screen.
//!
//! The pan and zoom are [`crate::canvas::View`] as well, for the reason given
//! there. This was drawn inside an `egui::Scene` once, and a `Scene` scales
//! the whole layer, glyphs included: a three-state machine fitted to a wide
//! pane was 11pt type stretched to three times its size, and the state names
//! came out smeared. Owning the transform means every label is laid out at the
//! size it is seen at, so the type is as sharp at 2× as at 1×.
//!
//! The visual language says three things a list cannot. A state is a pill, not
//! a card, so it never reads as a block in a block diagram. Colour marks only
//! the states whose fate is worth knowing — the reset state, one nothing
//! reaches, one nothing leaves — and everything else stays quiet. And a
//! transition's shape is its meaning: a straight run forward, a loop under the
//! row to go back, an arc over the top to stay put.

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2};
use rtlscope_graph::state::{EdgeShape, StateGeom};
use rtlscope_ir::Span;

use crate::canvas::{LEGIBLE, Placement};
use crate::theme::Theme;

/// How far from a line a click still counts, in pixels. A one-pixel arrow is
/// not something anyone can point at.
const PICK_SLOP: f32 = 4.0;
/// How long the arrow head is, in sheet units.
const ARROW: f32 = 7.0;
/// The arrow from nowhere into the reset state, in sheet units. Long enough
/// for the word `reset` to sit over it.
const RESET_STUB: f32 = 26.0;
/// Guards are conditions, sometimes long ones. Past this they are cut and the
/// whole thing appears when the arrow is pointed at. The number is the gap
/// between two layers divided by the width of this font: a guard any longer
/// would be written across the state it points at.
const GUARD_CHARS: usize = 18;
/// How far a fit may magnify a machine.
///
/// Two, not the four the wheel allows. A small machine in a wide pane is still
/// a diagram at 2× and a poster at 4×; the wheel goes the rest of the way for a
/// reader who wants it.
const FIT_MAX: f32 = 2.0;
// The ceiling only means something below what the wheel can reach.
const _: () = assert!(FIT_MAX < crate::canvas::ZOOM_MAX);

/// What the reader did to the diagram this frame.
#[derive(Debug, Default)]
pub struct StatesAction {
    pub hovered: Option<usize>,
    pub selected: Option<usize>,
    /// Somewhere to open in the source: the `case` arm a state was declared
    /// in, or the assignment a transition was written as.
    pub source: Option<Span>,
}

/// Draws the machine and reports what was done to it.
///
/// `placement` is where the drawing was left last frame, and is written back
/// with where it is now; a fresh one fits the machine to the pane.
pub fn draw(
    ui: &mut Ui,
    geom: &StateGeom,
    selected: Option<usize>,
    now: Option<usize>,
    placement: &mut Placement,
) -> StatesAction {
    let theme = Theme::of(ui);
    let mut action = StatesAction::default();

    // Claimed, not merely looked at, so the panel this sits in remembers the
    // height it was dragged to — the same lesson the cone learnt.
    let viewport = ui.available_rect_before_wrap();
    let background = ui.allocate_rect(viewport, Sense::click_and_drag());
    let sheet = Rect::from_min_size(Pos2::ZERO, Vec2::new(geom.width as f32, geom.height as f32));
    // A little margin, so the outermost pills are not flush against the frame.
    let margin = sheet.width().max(sheet.height()) * 0.04;
    let mut view = placement.view(viewport, sheet.expand(margin), FIT_MAX);
    view.steer(ui, &background);

    let painter = ui.painter_at(viewport);
    painter.rect_filled(viewport, 0.0, theme.canvas_bg);

    // Strokes keep their weight relative to the pills, with a floor so a line
    // never thins out of sight and a ceiling so it never becomes a bar.
    let weight = view.scale.clamp(0.5, 2.0);
    let small = view.font(8.0);
    let font = view.font(11.0);

    // ---- transitions, under the states they join ----
    for (index, edge) in geom.edges.iter().enumerate() {
        let points: Vec<Pos2> = edge.points.iter().map(|p| view.at(p.x, p.y)).collect();

        let mut hovered = false;
        for (segment, pair) in points.windows(2).enumerate() {
            // A rect per segment: the bounding box of a routed edge is mostly
            // empty space, and that space belongs to whatever is behind it.
            let hit = Rect::from_two_pos(pair[0], pair[1]).expand(PICK_SLOP);
            if !viewport.intersects(hit) {
                continue;
            }
            let response =
                ui.interact(hit, ui.id().with(("fsm-edge", index, segment)), Sense::click());
            hovered |= response.hovered();
            if response.clicked() {
                action.source = Some(edge.span);
            }
        }

        // An edge of the selected state is part of what that state does, so it
        // lights with it.
        let lit = selected.is_some_and(|at| edge.from == at || edge.to == at);
        let colour = if hovered || lit {
            theme.highlight
        } else if edge.shape != EdgeShape::Forward {
            // Everything that is not progress: the loops back and the moves
            // sideways would otherwise dominate a picture whose subject is
            // how the machine gets from reset to done.
            theme.muted
        } else {
            theme.wire
        };
        let width = if hovered || lit { 2.2 } else { 1.2 } * weight;
        for pair in points.windows(2) {
            painter.line_segment([pair[0], pair[1]], Stroke::new(width, colour));
        }
        arrow_head(&painter, &points, colour, ARROW * weight);

        if !edge.label.is_empty() && small.size >= LEGIBLE {
            let text = if hovered { edge.label.clone() } else { shorten(&edge.label) };
            guard_label(
                &painter,
                view.at(edge.label_at.x, edge.label_at.y),
                &text,
                small.clone(),
                if hovered || lit { theme.highlight } else { theme.muted },
                theme.canvas_bg,
            );
        }
    }

    // ---- the states ----
    for (at, state) in geom.states.iter().enumerate() {
        let rect = view.rect(state.rect.x, state.rect.y, state.rect.width, state.rect.height);
        let response = ui.interact(rect, ui.id().with(("fsm-state", at)), Sense::click());
        let is_selected = selected == Some(at);

        // Only the states whose fate is worth knowing get a colour, and it is
        // the same colour the badges above the diagram use.
        let (stroke_colour, fill) = if state.unreachable {
            (theme.warn, theme.warn_soft)
        } else if state.terminal {
            (theme.err, theme.err_soft)
        } else if state.is_reset {
            (theme.accent, theme.accent_soft)
        } else {
            (theme.box_stroke, theme.box_fill)
        };
        let stroke_width = if is_selected {
            2.4
        } else if response.hovered() {
            1.8
        } else {
            1.0
        } * weight;

        // A stadium, because that is what a state has been drawn as since
        // before any of this was on a screen.
        let rounding = CornerRadius::same((rect.height() / 2.0).clamp(0.0, 127.0) as u8);
        painter.rect_filled(rect, rounding, fill);
        painter.rect_stroke(
            rect,
            rounding,
            Stroke::new(stroke_width, if is_selected { theme.highlight } else { stroke_colour }),
            egui::StrokeKind::Inside,
        );

        // Where the machine is, at the waveform's cursor.
        //
        // A ring outside the box rather than another colour on it, because a
        // state can be three things at once and each answers a different
        // question: what kind of state it is (the fill), whether the reader
        // picked it (the thicker stroke), and whether the machine is in it
        // right now. A colour would have made the third overwrite the first.
        //
        // In the cursor's own colour, so the tie to the waveform is the thing
        // the eye makes rather than something the reader has to be told.
        if now == Some(at) {
            let ring = rect.expand(3.0 * weight);
            painter.rect_stroke(
                ring,
                CornerRadius::same((ring.height() / 2.0).clamp(0.0, 127.0) as u8),
                Stroke::new(2.0 * weight, theme.cursor),
                egui::StrokeKind::Outside,
            );
        }
        if font.size >= LEGIBLE {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                &state.name,
                font.clone(),
                theme.ink,
            );
        }

        // The ways in that come from outside the picture: the stub every state
        // diagram draws into its start state, so which one it is does not
        // depend on noticing a colour, and the one saying a `default:` arm
        // lands here. Both mean "from somewhere this drawing does not hold",
        // so both are drawn alike and told apart by their word.
        //
        // Parted when a state is the target of both, which is the ordinary
        // shape rather than a corner case: a `default:` most often puts the
        // machine back where reset does.
        let (reset_y, other_y) = match (state.is_reset, state.from_any_other) {
            (true, true) => {
                let apart = rect.height() * 0.24;
                (rect.center().y - apart, rect.center().y + apart)
            }
            _ => (rect.center().y, rect.center().y),
        };
        let length = RESET_STUB * view.scale;
        if state.is_reset {
            let at = Pos2::new(rect.left(), reset_y);
            stub(&painter, at, length, weight, "reset", theme.accent, &small, true);
        }
        if state.from_any_other {
            // The same accent the reset stub uses, because these are the same
            // kind of fact and the words tell them apart. Grey would have made
            // it a third thing the eye has to sort out from the transition
            // arrowheads, which converge on this very edge; and `warn` would
            // have put a warning colour on a machine written well.
            //
            // `default` rather than the analysis's `(any other)`: it is the
            // word the reader wrote in the `case`, and it is short enough to
            // sit under the arrow instead of across the lane beside it.
            let at = Pos2::new(rect.left(), other_y);
            stub(&painter, at, length, weight, "default", theme.accent, &small, false);
        }

        if response.hovered() {
            action.hovered = Some(at);
        }
        if response.clicked() {
            action.selected = Some(at);
        }
        if response.double_clicked() {
            action.source = Some(state.span);
        }
    }

    placement.remember(&view, viewport);
    action
}

/// A guard, on its own ground.
///
/// Guards sit in the same channels the arrows run through, and two of them can
/// land on the same square of canvas. Painting the sheet colour behind each one
/// means the top one is still readable rather than both being mush — the
/// diagram admits the collision instead of hiding it.
fn guard_label(
    painter: &egui::Painter,
    at: Pos2,
    text: &str,
    font: FontId,
    ink: Color32,
    ground: Color32,
) {
    let galley = painter.layout_no_wrap(text.to_owned(), font, ink);
    let size = galley.size();
    // Anchored centre-bottom, the way the geometry means it.
    let min = Pos2::new(at.x - size.x / 2.0, at.y - size.y);
    painter.rect_filled(
        Rect::from_min_size(min, size).expand2(Vec2::new(2.0, 0.5)),
        CornerRadius::same(2),
        ground,
    );
    painter.galley(min, galley, ink);
}

/// A filled triangle at the end of a polyline, pointing the way the last
/// segment runs. `length` is in pixels.
/// An arrow into the left edge of a state, from outside the picture.
///
/// `at` is the tip, on the box; the tail is `length` to its left. The word
/// goes above the arrow or below it rather than beside it, because it is about
/// as wide as the arrow is long and written on it neither can be read.
#[allow(clippy::too_many_arguments)]
fn stub(
    painter: &egui::Painter,
    at: Pos2,
    length: f32,
    weight: f32,
    label: &str,
    colour: Color32,
    font: &FontId,
    above: bool,
) {
    let tail = Pos2::new(at.x - length, at.y);
    painter.line_segment([tail, at], Stroke::new(1.6 * weight, colour));
    arrow_head(painter, &[tail, at], colour, ARROW * weight);
    if font.size >= LEGIBLE {
        let (align, dy) = match above {
            true => (Align2::CENTER_BOTTOM, -4.0 * weight),
            false => (Align2::CENTER_TOP, 4.0 * weight),
        };
        painter.text(
            Pos2::new((tail.x + at.x) / 2.0, at.y + dy),
            align,
            label,
            font.clone(),
            colour,
        );
    }
}

fn arrow_head(painter: &egui::Painter, points: &[Pos2], colour: Color32, length: f32) {
    let [.., before, tip] = points else { return };
    let direction = (*tip - *before).normalized();
    if !direction.x.is_finite() || !direction.y.is_finite() {
        return;
    }
    let across = Vec2::new(-direction.y, direction.x) * (length * 0.42);
    let base = *tip - direction * length;
    painter.add(Shape::convex_polygon(
        vec![*tip, base + across, base - across],
        colour,
        Stroke::NONE,
    ));
}

fn shorten(text: &str) -> String {
    if text.chars().count() <= GUARD_CHARS {
        return text.to_owned();
    }
    text.chars().take(GUARD_CHARS - 1).chain(std::iter::once('…')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_guard_is_cut_but_never_mid_character() {
        assert_eq!(shorten("a && b"), "a && b");
        let long = "state_q == S_IDLE && ready && !flush && counter == LIMIT";
        let cut = shorten(long);
        assert_eq!(cut.chars().count(), GUARD_CHARS);
        assert!(cut.ends_with('…'));
        assert!(long.starts_with(&cut[..cut.len() - '…'.len_utf8()]));
    }
}
