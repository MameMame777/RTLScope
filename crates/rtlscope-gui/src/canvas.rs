//! Painting a [`DiagramGeom`] with `egui`.
//!
//! The same geometry the SVG writer consumes, so a layout bug looks identical
//! in both and neither renderer can drift.
//!
//! The pan and zoom are worked out here rather than by [`egui::Scene`], which
//! this used to sit inside. A `Scene` transforms the whole layer, and a
//! transformed layer scales its text as a picture — glyphs rasterised for one
//! size stretched to another, which egui documents as blurring and which is
//! exactly what a diagram full of small labels cannot afford. Doing the
//! arithmetic here means every label is laid out at the size it will be seen
//! at, so the type stays as sharp at 4× as at 1×.
//!
//! Two more things follow from owning the transform. The wheel zooms, which is
//! what a hand expects of a diagram and what a `Scene` spends on panning. And
//! the text is painted after everything else — a label under a wire is a label
//! nobody can read, and draw order is only a choice once the drawing is not
//! being done by a container.
//!
//! Interaction is still `egui`'s: a rect per box, per wire segment and per pin,
//! handed over in screen coordinates. The rects are registered *before* their
//! shapes are painted, so hovering something changes how it is drawn in the
//! same frame the pointer arrives.
//!
//! The visual language is deliberately small: module boxes are rectangles,
//! ports are pills, and colour carries exactly three meanings — the accent for
//! "this is what you are pointing at", warm for clocks and resets, and the
//! blackbox tint for "RTLScope has no source for this". Everything else stays
//! quiet so those three read.

use std::collections::HashSet;

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use rtlscope_graph::block::{BlockNode, WireKind};
use rtlscope_graph::geom::{BoxKind, DiagramGeom, Side};
use rtlscope_ir::{NetId, Span};

use crate::theme::Theme;

/// How far from a wire or a pin a click still counts.
///
/// A one-pixel line is not something anyone can hit; this is the margin that
/// makes the diagram clickable without swallowing clicks meant for a box.
const PICK_SLOP: f32 = 3.0;

/// The spacing of the background grid, which exists to give panning a sense of
/// motion and scale — not to be seen.
const GRID: f32 = 64.0;

/// How far the wheel moves the zoom per notch.
///
/// Multiplicative, because zoom is a ratio: a step should feel the same at 4×
/// as at 0.1×, which adding a constant does not.
pub(crate) const ZOOM_PER_NOTCH: f32 = 1.0015;

/// How small a label may be drawn before it is left out.
///
/// Below this it is a grey smudge that says something is written without
/// saying what, which is worse than the gap: the gap at least reads as "zoom
/// in". Measured against the 8pt labels, so a diagram at 0.5× still has them.
pub(crate) const LEGIBLE: f32 = 5.0;

/// The map from the sheet's coordinates to the screen's.
///
/// A uniform scale and an offset, kept as those two numbers rather than as a
/// matrix because that is all a diagram needs and because both are wanted
/// separately: the scale multiplies every font size, the offset moves under a
/// drag.
#[derive(Debug, Clone, Copy)]
pub(crate) struct View {
    /// Where the sheet's origin lands on screen.
    pub(crate) origin: Pos2,
    /// Screen pixels per sheet unit.
    pub(crate) scale: f32,
}

impl View {
    /// The view that shows `camera` inside `viewport`.
    ///
    /// The smaller of the two ratios, so the whole of the camera fits rather
    /// than the wider half of it filling the frame.
    pub(crate) fn of(viewport: Rect, camera: Rect) -> View {
        View::fit(viewport, camera, ZOOM_MAX)
    }

    /// The same, with a ceiling on how far the fit may magnify.
    ///
    /// Fitting is right for a picture and wrong for a small one: three states
    /// fitted to a pane two thousand pixels wide are a headline, not a
    /// diagram. The wheel can still go past the ceiling — it limits what the
    /// fit chooses, not what the reader may.
    pub(crate) fn fit(viewport: Rect, camera: Rect, max_scale: f32) -> View {
        let scale = (viewport.width() / camera.width())
            .min(viewport.height() / camera.height())
            .clamp(ZOOM_MIN, max_scale.clamp(ZOOM_MIN, ZOOM_MAX));
        View { origin: viewport.center() - camera.center().to_vec2() * scale, scale }
    }

    pub(crate) fn at(&self, x: f64, y: f64) -> Pos2 {
        self.origin + Vec2::new(x as f32, y as f32) * self.scale
    }

    pub(crate) fn rect(&self, x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect::from_min_size(self.at(x, y), Vec2::new(width as f32, height as f32) * self.scale)
    }

    /// A font size in sheet units, as one in pixels.
    ///
    /// Rounded, because egui rasterises a face once per size and a continuous
    /// zoom would otherwise ask it for a hundred nearly identical ones.
    pub(crate) fn font(&self, points: f32) -> FontId {
        FontId::monospace((points * self.scale).round().max(1.0))
    }

    /// Keeps the sheet point under `anchor` where it is while the scale changes.
    pub(crate) fn zoom(&mut self, factor: f32, anchor: Pos2) {
        let want = (self.scale * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        let factor = want / self.scale;
        self.origin = anchor + (self.origin - anchor) * factor;
        self.scale = want;
    }

    /// What the viewport is showing, back in the sheet's own coordinates.
    ///
    /// Kept in that form because it is what gets written to the session file:
    /// a region of the diagram survives the window being resized, where a pixel
    /// offset does not.
    pub(crate) fn camera(&self, viewport: Rect) -> Rect {
        let back = |at: Pos2| ((at - self.origin) / self.scale).to_pos2();
        Rect::from_min_max(back(viewport.min), back(viewport.max))
    }

    /// The wheel, a pinch and a drag, applied. Says whether any of them
    /// happened.
    ///
    /// `background` is the response of the whole drawing surface. Zooming only
    /// while it is hovered keeps a wheel over a neighbouring panel from moving
    /// the picture; the pan follows a drag wherever it started, since a box
    /// senses only clicks and dragging from one should still move the drawing,
    /// which is what a hand does. Shared by every drawing that owns its
    /// transform, so they all answer the same gestures the same way.
    pub(crate) fn steer(&mut self, ui: &Ui, background: &egui::Response) -> bool {
        let mut moved = false;
        if background.hovered() {
            let notches = ui.input(|input| input.smooth_scroll_delta.y);
            if notches != 0.0
                && let Some(pointer) = ui.input(|input| input.pointer.hover_pos())
            {
                self.zoom(ZOOM_PER_NOTCH.powf(notches), pointer);
                moved = true;
            }
            // A pinch on a trackpad means the same thing, and arrives separately.
            let pinch = ui.input(|input| input.zoom_delta());
            if pinch != 1.0
                && let Some(pointer) = ui.input(|input| input.pointer.hover_pos())
            {
                self.zoom(pinch, pointer);
                moved = true;
            }
        }
        if background.dragged() {
            self.origin += background.drag_delta();
            moved = true;
        }
        moved
    }
}

/// Where a drawing has been put, kept between frames.
///
/// A scale and an offset from the top-left of whatever it is drawn in — not a
/// region of the sheet. A region has to be fitted back into a viewport to be
/// used, and fitting is lossy whenever the proportions differ: read it out and
/// hand it back in and the picture creeps. Measured twice on this project, on
/// windows and then on the cone, where dragging the pane taller shrank the
/// drawing away to nothing. A scale and an offset are what the drawing already
/// is, so nothing has to be recovered from them.
///
/// `None` means "not placed yet", which is the request to fit.
#[derive(Debug, Default, Clone, Copy)]
pub struct Placement {
    at: Option<(f32, Vec2)>,
}

impl Placement {
    /// Forgets where it was, so the next frame fits it again.
    pub fn refit(&mut self) {
        self.at = None;
    }

    /// The view this means inside `viewport` — or, when nothing has been
    /// placed yet, a fit of `sheet` magnified no more than `max_scale`.
    pub(crate) fn view(&self, viewport: Rect, sheet: Rect, max_scale: f32) -> View {
        match self.at {
            Some((scale, offset)) => View { origin: viewport.min + offset, scale },
            None => View::fit(viewport, sheet, max_scale),
        }
    }

    /// Writes a view down as what it is.
    pub(crate) fn remember(&mut self, view: &View, viewport: Rect) {
        self.at = Some((view.scale, view.origin - viewport.min));
    }
}

/// The zoom this diagram may be looked at through.
///
/// The far end is 4×: past that a diagram is one box and a reader has lost the
/// structure they came for. The near end is small enough to take in a design
/// nobody could read at 1:1.
const ZOOM_MIN: f32 = 0.02;
pub(crate) const ZOOM_MAX: f32 = 4.0;

/// A label, held back until everything else is drawn.
///
/// Text goes last. A name half under a wire is a name nobody can read, and
/// which of the two happens to be painted second is not something to leave to
/// the order the geometry came in.
struct Label {
    at: Pos2,
    align: Align2,
    text: String,
    font: FontId,
    colour: Color32,
    /// Painted on the sheet colour, for a name that has to sit across other
    /// wires and would otherwise read as one more line.
    halo: bool,
}

/// Room left either side of a wire's name before it counts as fitting its run,
/// and the margin of sheet colour painted behind one that does not.
const LABEL_PAD: f32 = 3.0;

/// How far above its wire a name sits, in sheet units.
const LABEL_LIFT: f32 = 4.0;

/// One straight piece of a wire, in sheet units, with its ends in order.
struct Piece {
    wire: usize,
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
}

/// The rectangle a wire's name would occupy, in sheet units.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Footprint {
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
}

impl Footprint {
    /// A name `width` by `height`, centred on `centre` and lifted off the wire
    /// at `y`, with `pad` either side.
    fn of(centre: f64, y: f64, width: f64, height: f64, pad: f64) -> Self {
        let lift = f64::from(LABEL_LIFT);
        Footprint {
            x0: centre - width / 2.0 - pad,
            x1: centre + width / 2.0 + pad,
            y0: y - lift - height,
            y1: y - lift,
        }
    }

    fn overlaps(&self, other: &Footprint) -> bool {
        self.x0 <= other.x1 && other.x0 <= self.x1 && self.y0 <= other.y1 && other.y0 <= self.y1
    }

    /// Whether no piece of any wire but `own` passes through it.
    fn clear_of(&self, pieces: &[Piece], own: usize) -> bool {
        !pieces.iter().any(|piece| {
            piece.wire != own
                && self.overlaps(&Footprint {
                    x0: piece.x0,
                    x1: piece.x1,
                    y0: piece.y0,
                    y1: piece.y1,
                })
        })
    }
}

/// Whether a name fits along the run of wire it would be written over.
///
/// Both in pixels. A name wider than its run is written across the neighbouring
/// wires, and a diagram of a bus is mostly neighbouring wires.
fn label_fits(text_width: f32, run: f32) -> bool {
    text_width + 2.0 * LABEL_PAD <= run
}

/// One pin of one box, as much of it as an edit needs.
///
/// What the user did to the canvas this frame.
#[derive(Debug, Default)]
pub struct CanvasAction {
    /// A box was clicked: select it and show where it came from.
    pub selected: Option<BlockNode>,
    /// A box was double-clicked: step into it if it has an inside.
    pub entered: Option<BlockNode>,
    /// Where that box came from in the source. Carried alongside because a box
    /// that cannot be stepped into shows its RTL instead, and the geometry
    /// already knows the span — looking it up again would be a second answer to
    /// a question already answered.
    pub entered_span: Option<Span>,
    /// A wire or a pin was clicked. The diagram is where a signal is easiest
    /// to find, so this is how one gets from the picture to the waveform.
    pub picked_net: Option<NetId>,
}

/// Draws the diagram and reports what the user did to it.
///
/// `camera` is the region of the sheet on screen, in the sheet's own units. It
/// comes in as what was shown last time and goes out as what is shown now, so
/// panning and zooming are just this function editing it. An empty one means
/// "fit", which is how the toolbar's `fit` and a freshly opened module ask.
pub fn draw(
    ui: &mut Ui,
    geom: &DiagramGeom,
    camera: &mut Rect,
    show_clocks: bool,
    selected: Option<BlockNode>,
    highlighted: &HashSet<NetId>,
) -> CanvasAction {
    let theme = Theme::of(ui);
    let sheet = Rect::from_min_size(Pos2::ZERO, Vec2::new(geom.width as f32, geom.height as f32));

    // The whole canvas, claimed and sensed in one: everything drawn on it is
    // added afterwards and so sits on top and takes its clicks first. The drag
    // is the pan — a box senses only clicks, so dragging from one still moves
    // the diagram, which is what a hand does.
    //
    // Claimed rather than only measured, because a container that sizes itself
    // to its content is entitled to believe a view that takes no space needs
    // none.
    let viewport = ui.available_rect_before_wrap();
    let id = ui.advance_cursor_after_rect(viewport);
    // Claimed to the edge, but sensed a little short of it, so the seams beside
    // it stay catchable. The dock registers its separators after every pane's
    // contents precisely so they win the band, which is the fix this used to be
    // — under the old fixed panels the canvas was drawn last and ate the outer
    // half of every resize handle, and a press one pixel above the seam panned
    // the drawing instead of moving it. The margin stays anyway: it costs a few
    // pixels of a drawing that pans, and it is also what keeps a press near the
    // edge of a floating window from panning instead of resizing.
    let grab = ui.style().interaction.resize_grab_radius_side;
    let background = ui.interact(viewport.shrink(grab), id, Sense::click_and_drag());

    // An empty camera is the request to fit. A little margin, so the outermost
    // boxes are not flush against the frame.
    let mut view = match camera.is_positive() {
        true => View::of(viewport, *camera),
        false => View::of(viewport, sheet.expand(sheet.width().max(sheet.height()) * 0.02)),
    };

    let mut moved = !camera.is_positive();
    moved |= view.steer(ui, &background);

    let painter = ui.painter_at(viewport);
    painter.rect_filled(viewport, 0.0, theme.canvas_bg);
    grid(&painter, &view, sheet, viewport, theme.grid);

    // Every label, gathered as the shapes are drawn and painted after all of
    // them.
    let mut labels: Vec<Label> = Vec::new();
    let mut picked: Option<NetId> = None;
    // Every straight piece of every drawn wire, for asking what a name would
    // lie across before it is written; and the names already placed, so two
    // do not land on one another.
    let pieces: Vec<Piece> = geom
        .wires
        .iter()
        .enumerate()
        .filter(|(_, wire)| show_clocks || wire.kind == WireKind::Signal)
        .flat_map(|(index, wire)| {
            wire.points.windows(2).map(move |pair| Piece {
                wire: index,
                x0: pair[0].x.min(pair[1].x),
                x1: pair[0].x.max(pair[1].x),
                y0: pair[0].y.min(pair[1].y),
                y1: pair[0].y.max(pair[1].y),
            })
        })
        .collect();
    let mut occupied: Vec<Footprint> = Vec::new();

    for (index, wire) in geom.wires.iter().enumerate() {
        if !show_clocks && wire.kind != WireKind::Signal {
            continue;
        }
        let points: Vec<Pos2> = wire.points.iter().map(|p| view.at(p.x, p.y)).collect();

        // The hit test first, so a hovered wire is drawn hovered this frame.
        let mut hovered = false;
        for (segment, pair) in points.windows(2).enumerate() {
            // A rect per segment rather than one around the polyline: an
            // L-shaped run's bounding box is mostly empty space, and clicking
            // that empty space should not pick the wire.
            let hit = Rect::from_two_pos(pair[0], pair[1]).expand(PICK_SLOP);
            if !viewport.intersects(hit) {
                continue;
            }
            let response = ui.interact(hit, ui.id().with(("wire", index, segment)), Sense::click());
            hovered |= response.hovered();
            if response.clicked() {
                picked = Some(wire.net);
            }
        }

        let lit = highlighted.contains(&wire.net);
        let colour = if lit {
            theme.highlight
        } else {
            match wire.kind {
                WireKind::Signal => theme.wire,
                WireKind::Clock => theme.clock,
                WireKind::Reset => theme.reset,
            }
        };
        // Widths are in sheet units so a wire keeps its weight relative to the
        // boxes, with a floor so it never thins out of sight.
        let width = if lit {
            2.4
        } else if hovered {
            2.0
        } else {
            1.2
        } * view.scale.clamp(0.5, 2.0);
        for pair in points.windows(2) {
            painter.line_segment([pair[0], pair[1]], Stroke::new(width, colour));
        }

        if let Some((x0, x1, y)) = label_run(wire)
            && (lit || hovered || wire.kind == WireKind::Signal)
        {
            let font = view.font(8.0);
            if font.size >= LEGIBLE {
                // A name goes where nothing else is. Between two boxes joined
                // by twenty wires every run is crossed by the others' trunks,
                // and a name written across them was one more line to read
                // through. So the middle of the run is tried, then either end,
                // and a name with nowhere clear to go is not written — the pins
                // on both boxes already say what each wire is. Hovering or
                // lighting the wire brings its name back regardless, on the
                // sheet colour, so it can be read across the others.
                let galley = painter.layout_no_wrap(wire.label.clone(), font.clone(), theme.muted);
                let width = f64::from(galley.size().x / view.scale);
                let height = f64::from(galley.size().y / view.scale);
                let pad = f64::from(LABEL_PAD / view.scale);
                let mid = (x0 + x1) / 2.0;
                let colour = if lit || hovered { theme.highlight } else { theme.muted };
                let spot = label_fits(galley.size().x, (x1 - x0) as f32 * view.scale)
                    .then(|| {
                        [mid, x0 + pad + width / 2.0, x1 - pad - width / 2.0].into_iter().find(
                            |centre| {
                                let footprint = Footprint::of(*centre, y, width, height, pad);
                                footprint.clear_of(&pieces, index)
                                    && !occupied.iter().any(|taken| taken.overlaps(&footprint))
                            },
                        )
                    })
                    .flatten();
                match spot {
                    Some(centre) => {
                        occupied.push(Footprint::of(centre, y, width, height, pad));
                        labels.push(Label {
                            at: view.at(centre, y) - Vec2::new(0.0, LABEL_LIFT * view.scale),
                            align: Align2::CENTER_BOTTOM,
                            text: wire.label.clone(),
                            font,
                            colour,
                            halo: false,
                        });
                    }
                    None if lit || hovered => labels.push(Label {
                        at: view.at(mid, y) - Vec2::new(0.0, LABEL_LIFT * view.scale),
                        align: Align2::CENTER_BOTTOM,
                        text: wire.label.clone(),
                        font,
                        colour,
                        halo: true,
                    }),
                    None => {}
                }
            }
        }
    }

    let mut action = CanvasAction::default();

    for (index, node) in geom.boxes.iter().enumerate() {
        let rect = view.rect(node.rect.x, node.rect.y, node.rect.width, node.rect.height);
        if !viewport.intersects(rect.expand(PICK_SLOP * 4.0)) {
            continue;
        }
        let response = ui.interact(rect, ui.id().with(("box", index)), Sense::click());
        let is_selected = selected == Some(node.node);
        let is_port = matches!(node.kind, BoxKind::InputPort | BoxKind::OutputPort);

        let (fill, stroke_colour) = if node.blackbox {
            (theme.blackbox_fill, theme.blackbox_stroke)
        } else if is_selected {
            (theme.accent_soft, theme.accent)
        } else if is_port {
            (theme.port_fill, theme.muted)
        } else {
            (theme.box_fill, theme.box_stroke)
        };
        let stroke_width = if is_selected {
            2.0
        } else if response.hovered() {
            1.6
        } else {
            1.0
        } * view.scale.clamp(0.5, 2.0);

        // A port is a pill, a module or a register is a card, and logic with
        // no state is a cloud: the shape says what a box is before its label
        // does, and a reader scanning for where the clock stops can see it.
        if node.comb {
            cloud(&painter, &view, &node.rect, fill, Stroke::new(stroke_width, stroke_colour));
        } else {
            let rounding = if is_port {
                CornerRadius::same((rect.height() / 2.0).min(127.0) as u8)
            } else {
                CornerRadius::same((4.0 * view.scale).clamp(1.0, 127.0) as u8)
            };
            painter.rect_filled(rect, rounding, fill);
            painter.rect_stroke(
                rect,
                rounding,
                Stroke::new(stroke_width, stroke_colour),
                egui::StrokeKind::Inside,
            );
        }

        let font = view.font(11.0);
        if font.size >= LEGIBLE {
            labels.push(Label {
                at: Pos2::new(
                    rect.center().x,
                    if is_port { rect.center().y } else { rect.top() + 3.0 * view.scale },
                ),
                align: if is_port { Align2::CENTER_CENTER } else { Align2::CENTER_TOP },
                text: node.label.clone(),
                font,
                colour: theme.ink,
                halo: false,
            });
        }
        let small = view.font(8.0);
        if let Some(sublabel) = &node.sublabel
            && !is_port
            && small.size >= LEGIBLE
        {
            labels.push(Label {
                at: Pos2::new(rect.center().x, rect.bottom() - 3.0 * view.scale),
                align: Align2::CENTER_BOTTOM,
                text: sublabel.clone(),
                font: small.clone(),
                colour: theme.muted,
                halo: false,
            });
        }
        // The module is only half understood, and the box says so (D2).
        if node.skipped > 0 && small.size >= LEGIBLE {
            labels.push(Label {
                at: Pos2::new(rect.right() - 3.0 * view.scale, rect.top() + 3.0 * view.scale),
                align: Align2::RIGHT_TOP,
                text: format!("{} skipped", node.skipped),
                font: small.clone(),
                colour: theme.warn,
                halo: false,
            });
        }

        for (which, pin) in node.pins.iter().enumerate() {
            let at = view.at(pin.at.x, pin.at.y);
            let lit = pin.net.is_some_and(|net| highlighted.contains(&net));

            // A pin is the smallest place a net can be picked, and the one a
            // reader points at when they mean "this port".
            //
            // Every pin gets a hit region, connected or not. This used to be
            // inside `if let Some(net)`, which left an unconnected pin with no
            // region at all — and an unconnected port is precisely the one
            // someone reaches for when they mean to wire it up.
            //
            // Clicks only: the pan lives on the background now, so a drag
            // begun on a pin moves the diagram like a drag begun anywhere
            // else. Under a `Scene` this had to sense drags itself to stop the
            // whole layer sliding out from under the press.
            let hit = Rect::from_center_size(at, Vec2::splat(PICK_SLOP * 3.0));
            let pin_response =
                ui.interact(hit, ui.id().with(("pin", index, which)), Sense::click());
            let pin_hovered = pin_response.hovered();
            if pin_response.clicked()
                && let Some(net) = pin.net
            {
                picked = Some(net);
            }

            let (radius, colour) = if lit {
                (3.0, theme.highlight)
            } else if pin_hovered {
                (3.0, theme.accent)
            } else {
                (2.0, stroke_colour)
            };
            painter.circle_filled(at, radius * view.scale.clamp(0.5, 2.0), colour);
            if small.size >= LEGIBLE {
                let (dx, align) = match pin.side {
                    Side::Left => (4.0, Align2::LEFT_CENTER),
                    Side::Right => (-4.0, Align2::RIGHT_CENTER),
                };
                labels.push(Label {
                    at: Pos2::new(at.x + dx * view.scale, at.y),
                    align,
                    text: pin.name.clone(),
                    font: small.clone(),
                    colour: if lit || pin_hovered { theme.highlight } else { theme.muted },
                    halo: false,
                });
            }
        }

        if response.clicked() {
            action.selected = Some(node.node);
        }
        if response.double_clicked() {
            action.entered = Some(node.node);
            action.entered_span = Some(node.span);
        }
    }

    // Last, over everything.
    for label in labels {
        if label.halo {
            let galley =
                painter.layout_no_wrap(label.text.clone(), label.font.clone(), label.colour);
            let rect = label.align.anchor_size(label.at, galley.size());
            painter.rect_filled(rect.expand2(Vec2::new(LABEL_PAD, 1.0)), 2.0, theme.canvas_bg);
            painter.galley(rect.min, galley, label.colour);
        } else {
            painter.text(label.at, label.align, &label.text, label.font, label.colour);
        }
    }

    // Written back only when the reader moved it. `View::of` fits a camera
    // into a viewport, and `camera` reports what is visible — which for any
    // mismatch of proportions is *wider* than what was asked for. Feeding that
    // back in every frame grows the camera a little each time, and the drawing
    // shrinks steadily inside it. Measured after dragging the splitter: the
    // diagram walked down to a thumbnail. The same lesson as the windows —
    // never read a geometry back and hand it in as the next frame's input.
    if moved {
        *camera = view.camera(viewport);
    }
    // Set last: the pins are drawn with the boxes, so a pick can come from
    // after the wire loop.
    action.picked_net = picked;
    action
}

/// A combinational box, drawn as a cloud.
///
/// The bumps come from [`rtlscope_graph::geom::cloud`], the same ones the SVG
/// draws. The outline goes down first and the fill over it: every bump's inner
/// arc ends up under the body or under a neighbour, and only the outer envelope
/// is left showing — which is how a cloud has been drawn since pen and ink,
/// and spares working out where each arc meets the next.
fn cloud(
    painter: &egui::Painter,
    view: &View,
    rect: &rtlscope_graph::Rect,
    fill: Color32,
    stroke: Stroke,
) {
    let shape = rtlscope_graph::geom::cloud(rect);
    let radius = shape.radius as f32 * view.scale;
    let centres: Vec<Pos2> = shape.bumps.iter().map(|bump| view.at(bump.x, bump.y)).collect();
    for centre in &centres {
        painter.circle_stroke(*centre, radius, stroke);
    }
    // Half a stroke inside the outline, so the outer half of every bump's
    // stroke survives and the inner arcs do not.
    let inside = (radius - stroke.width * 0.5).max(0.5);
    for centre in &centres {
        painter.circle_filled(*centre, inside, fill);
    }
    let body = shape.body;
    painter.rect_filled(view.rect(body.x, body.y, body.width, body.height), 0.0, fill);
}

/// Faint lines, not dots: a dot grid at this density is thousands of shapes,
/// and lines are two per column however tall the sheet is.
fn grid(painter: &egui::Painter, view: &View, sheet: Rect, viewport: Rect, colour: Color32) {
    // Coarser as the diagram shrinks, so a sheet at 0.1× is not a solid wash.
    // Doubling rather than any step, so lines stay where they were and the
    // grid does not appear to slide when the step changes.
    let mut step = GRID;
    while step * view.scale < 8.0 {
        step *= 2.0;
    }

    let stroke = Stroke::new(0.5, colour);
    let mut x = step;
    while x < sheet.width() {
        let at = view.at(x as f64, 0.0).x;
        if at >= viewport.left() && at <= viewport.right() {
            let top = view.at(0.0, 0.0).y.max(viewport.top());
            let bottom = view.at(0.0, sheet.height() as f64).y.min(viewport.bottom());
            painter.line_segment([Pos2::new(at, top), Pos2::new(at, bottom)], stroke);
        }
        x += step;
    }
    let mut y = step;
    while y < sheet.height() {
        let at = view.at(0.0, y as f64).y;
        if at >= viewport.top() && at <= viewport.bottom() {
            let left = view.at(0.0, 0.0).x.max(viewport.left());
            let right = view.at(sheet.width() as f64, 0.0).x.min(viewport.right());
            painter.line_segment([Pos2::new(left, at), Pos2::new(right, at)], stroke);
        }
        y += step;
    }
}

/// The midpoint of the longest horizontal run, matching the SVG writer so the
/// two renderers put labels in the same place.
///
/// The run comes back as its two ends and its height, in sheet units, because
/// where along it the name goes — and whether it goes at all — is decided
/// against the other wires.
fn label_run(wire: &rtlscope_graph::Wire) -> Option<(f64, f64, f64)> {
    wire.points
        .windows(2)
        .filter(|pair| (pair[0].y - pair[1].y).abs() < 0.5)
        .max_by(|a, b| (a[0].x - a[1].x).abs().total_cmp(&(b[0].x - b[1].x).abs()))
        .filter(|pair| (pair[0].x - pair[1].x).abs() > 30.0)
        .map(|pair| (pair[0].x.min(pair[1].x), pair[0].x.max(pair[1].x), pair[0].y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> Rect {
        Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(800.0, 400.0))
    }

    /// The point of fitting is that the whole of it is on screen, which means
    /// the smaller of the two ratios. Taking the larger fills the frame with
    /// the wider half and hides the rest.
    #[test]
    fn fitting_shows_all_of_the_camera_and_centres_it() {
        // Twice as wide as the viewport in proportion, so height is not the
        // constraint and the horizontal ratio wins.
        let camera = Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 400.0));
        let view = View::of(viewport(), camera);

        assert!((view.scale - 0.5).abs() < 1e-4, "800 across 1600: {view:?}");
        let (min, max) = (view.at(0.0, 0.0), view.at(1600.0, 400.0));
        assert!((min.x - viewport().left()).abs() < 0.5, "flush left: {min:?}");
        assert!((max.x - viewport().right()).abs() < 0.5, "and right: {max:?}");
        assert!(min.y > viewport().top(), "with the slack shared top and bottom: {min:?}");
        assert!((viewport().center().y - (min.y + max.y) / 2.0).abs() < 0.5, "centred");
    }

    /// Zooming about the pointer means the thing under it stays under it. A
    /// zoom that drifts is one a reader has to chase with a pan.
    #[test]
    fn the_wheel_zooms_about_what_it_is_pointing_at() {
        let mut view = View::of(viewport(), Rect::from_min_size(Pos2::ZERO, Vec2::splat(800.0)));
        let anchor = Pos2::new(400.0, 200.0);
        let under = ((anchor - view.origin) / view.scale).to_pos2();

        view.zoom(2.0, anchor);
        let now = view.at(f64::from(under.x), f64::from(under.y));

        assert!((now - anchor).length() < 0.5, "it did not move: {now:?} vs {anchor:?}");
    }

    /// Neither end is a place worth being: past 4× the diagram is one box, and
    /// below the floor it is a smudge. Both are reachable, and neither is
    /// passable.
    #[test]
    fn the_zoom_stops_at_both_ends() {
        let mut view = View::of(viewport(), Rect::from_min_size(Pos2::ZERO, Vec2::splat(800.0)));
        for _ in 0..40 {
            view.zoom(2.0, viewport().center());
        }
        assert_eq!(view.scale, ZOOM_MAX);
        for _ in 0..40 {
            view.zoom(0.5, viewport().center());
        }
        assert_eq!(view.scale, ZOOM_MIN);
    }

    /// The camera is what gets written to the session file: a region of the
    /// sheet, which survives the window being resized where a pixel offset
    /// would not. So it has to come back as the same view it went out as.
    #[test]
    fn a_camera_read_back_is_the_view_it_came_from() {
        let first =
            View::of(viewport(), Rect::from_min_size(Pos2::new(20.0, 30.0), Vec2::splat(600.0)));
        let again = View::of(viewport(), first.camera(viewport()));

        assert!((first.scale - again.scale).abs() < 1e-3, "{first:?} then {again:?}");
        assert!((first.origin - again.origin).length() < 0.5, "{first:?} then {again:?}");
    }

    /// Text is laid out at the size it will be seen at, which is the whole
    /// reason this does its own transform: a scaled layer stretches glyphs
    /// rasterised for another size, and small labels cannot afford that.
    #[test]
    fn a_label_is_measured_in_the_pixels_it_will_occupy() {
        let mut view = View::of(viewport(), Rect::from_min_size(Pos2::ZERO, Vec2::splat(800.0)));
        view.scale = 2.0;
        assert_eq!(view.font(11.0).size, 22.0);

        // Rounded, because egui rasterises a face once per size and a
        // continuous zoom would otherwise ask for a hundred near-identical ones.
        view.scale = 1.03;
        assert_eq!(view.font(11.0).size, 11.0);
    }

    /// A wire's name is written along its own run only when it fits there;
    /// wider, it would lie across the neighbouring wires.
    #[test]
    fn a_name_wider_than_its_run_is_held_back() {
        assert!(label_fits(40.0, 60.0));
        assert!(!label_fits(40.0, 44.0), "the padding either side counts");
        assert!(!label_fits(90.0, 60.0));
    }

    /// A name is written only where nothing else is: another wire's trunk
    /// crossing the run blocks it, its own wire does not, and a name already
    /// there does.
    #[test]
    fn a_name_is_written_only_where_nothing_crosses_it() {
        let pieces = vec![
            // Its own run.
            Piece { wire: 0, x0: 0.0, x1: 100.0, y0: 50.0, y1: 50.0 },
            // Another wire's trunk, straight through the run.
            Piece { wire: 1, x0: 60.0, x1: 60.0, y0: 0.0, y1: 100.0 },
            // Another wire's run, well above.
            Piece { wire: 2, x0: 0.0, x1: 100.0, y0: 20.0, y1: 20.0 },
        ];
        let over_trunk = Footprint::of(50.0, 50.0, 40.0, 10.0, 2.0);
        assert!(!over_trunk.clear_of(&pieces, 0), "the trunk at x=60 passes through it");
        let beside = Footprint::of(20.0, 50.0, 30.0, 10.0, 2.0);
        assert!(beside.clear_of(&pieces, 0), "x 3..37, under nothing but its own wire");
        assert!(beside.overlaps(&Footprint::of(30.0, 50.0, 30.0, 10.0, 2.0)));
        assert!(!beside.overlaps(&Footprint::of(80.0, 50.0, 30.0, 10.0, 2.0)));
    }

    /// A fit may be capped: a small picture in a large pane is shown at the
    /// ceiling and centred, not blown up to fill the frame.
    #[test]
    fn a_capped_fit_stops_at_the_ceiling_and_stays_centred() {
        let camera = Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 50.0));
        let free = View::of(viewport(), camera);
        assert_eq!(free.scale, ZOOM_MAX, "uncapped, the fit runs to the zoom limit");

        let capped = View::fit(viewport(), camera, 2.0);
        assert_eq!(capped.scale, 2.0);
        let centre = capped.at(100.0, 25.0);
        assert!((centre - viewport().center()).length() < 0.5, "centred: {centre:?}");
    }

    /// A placement is a scale and an offset, so what is written down comes
    /// back exactly — nothing is fitted on the way, and nothing can drift.
    #[test]
    fn a_placement_comes_back_as_it_was_left() {
        let sheet = Rect::from_min_size(Pos2::ZERO, Vec2::splat(800.0));
        let mut placement = Placement::default();
        let mut view = placement.view(viewport(), sheet, ZOOM_MAX);
        view.zoom(1.7, Pos2::new(300.0, 120.0));
        view.origin += Vec2::new(13.0, -7.0);
        placement.remember(&view, viewport());

        // In a taller pane with the same corner: the same drawing, more of it.
        let taller = Rect::from_min_size(viewport().min, Vec2::new(800.0, 700.0));
        let again = placement.view(taller, sheet, ZOOM_MAX);
        assert_eq!(again.scale, view.scale);
        assert!((again.origin - view.origin).length() < 1e-3, "{again:?} vs {view:?}");

        placement.refit();
        let fitted = placement.view(viewport(), sheet, ZOOM_MAX);
        assert_eq!(fitted.scale, View::of(viewport(), sheet).scale, "forgotten, so fitted again");
    }
}
