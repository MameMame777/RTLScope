//! Drawing a waveform to run against the design.
//!
//! Every other view here answers a question about a design that already exists.
//! This one is the opposite: it is where somebody *says* something — that when
//! `valid` goes up here, this should come back there — and saying it is what
//! makes a testcase. What the grid holds is a [`rtlscope_tb::Pattern`], and
//! running it produces a verdict rather than a picture.
//!
//! The grid is deliberately not the waveform panel. There is no zooming and no
//! panning: a column is a fixed width because a column is a *clock cycle*, and
//! the whole point is to point at one. The vocabulary is shared, though —
//! one-bit rows are drawn as a waveform and buses as a run of boxes with the
//! value in them — so what is drawn here and what comes back can be read the
//! same way.
//!
//! Two rules keep the drawing honest, both from [`rtlscope_tb::pattern`]:
//!
//! - **Column `N` is the span between rising edge `N` and rising edge `N+1`.**
//!   The drive row and the expect row of one column talk about the same moment.
//! - **An undrawn expectation is not an expectation of zero.** Expect cells
//!   start at don't-care and are drawn into deliberately; a row nobody touched
//!   claims nothing.

use egui::{Align2, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use rtlscope_tb::pattern::{Cell, Lane, Pattern};

use crate::theme::{Theme, badge};

/// How wide one clock cycle is drawn. Fixed: a column is a cycle, and a cycle
/// is not something to zoom.
const COLUMN: f32 = 34.0;
/// How tall one port's row is.
const ROW: f32 = 26.0;
/// How wide the names down the left are.
const NAMES: f32 = 150.0;

/// What the reader did to the drawing.
#[derive(Debug, Default)]
pub struct StimAction {
    /// Play it and see.
    pub run: bool,
    /// Put the waveform's cursor on this column's moment.
    pub seek: Option<u64>,
    /// Start again from an empty drawing.
    pub clear: bool,
    /// Write it beside the design, so it outlives the window.
    pub save: bool,
    /// Read back what was written.
    pub load: bool,
    /// Something to say about what was just typed, for the status bar.
    ///
    /// A value that will not fit the port is refused rather than narrowed, and
    /// a refusal nobody is told about reads as a cell that would not take a
    /// number — so the reason has to leave this pane.
    pub said: Option<String>,
}

/// Where a cell is in the drawing: which half, which row, which column.
type At = (bool, usize, u64);

/// Which cell is being typed into, and what has been typed.
#[derive(Debug, Default)]
pub struct StimPane {
    /// The row and column of the cell with an entry open, and whether the row
    /// is an expectation.
    editing: Option<At>,
    entry: String,
    /// A cell that was clicked, opened once the rows have all been walked.
    ///
    /// Not opened where the click lands: the cell already being edited may come
    /// later in the same pass, and overwriting `editing` and `entry` there would
    /// throw away what was typed into it before its own row is reached.
    wants: Option<At>,
    /// Whether the open entry still has to be handed the keyboard.
    ///
    /// Once, not every frame. egui counts focus as lost only when nothing asked
    /// for it during the frame, so asking again on every pass made
    /// `lost_focus()` permanently false — and that is the only place a typed
    /// value was taken, so a bus could be typed into and never committed.
    taking: bool,
    /// The value a drag is painting, so the whole run gets what the first cell
    /// got rather than each cell toggling under the pointer.
    painting: Option<Cell>,
    /// Why the last thing typed was not taken, on its way to the status bar.
    said: Option<String>,
}

/// Draws the grid and reports what was done to it.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    pane: &mut StimPane,
    pattern: &mut Pattern,
    verdict: Option<&rtlscope_tb::Verdict>,
    running: Option<&str>,
    refused: Option<&str>,
    problems: &[String],
) -> StimAction {
    let theme = Theme::of(ui);
    let mut action = StimAction::default();

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{}.{}", pattern.module, pattern.clock)).monospace().strong(),
        );
        ui.label(
            RichText::new(format!("{} cycle(s) of {} ns", pattern.cycles, pattern.period_ns))
                .small()
                .weak(),
        );
        ui.separator();

        match running {
            Some(what) => {
                ui.spinner();
                ui.label(RichText::new(what).weak());
            }
            None => {
                if ui
                    .button("run")
                    .on_hover_text("Play this drawing and check what it expects")
                    .clicked()
                {
                    action.run = true;
                }
            }
        }
        if ui.button("+16 cycles").clicked() {
            pattern.cycles += 16;
        }

        // A drawing is the one thing in this window somebody made rather than
        // derived, and until it is written down it exists only here.
        if ui
            .button("save")
            .on_hover_text(
                "Write this drawing beside the design. `rtlscope sim --pattern` reads it",
            )
            .clicked()
        {
            action.save = true;
        }
        if ui.button("load").on_hover_text("Read the saved drawing back").clicked() {
            action.load = true;
        }
        if ui.button("clear").on_hover_text("Start again from an empty drawing").clicked() {
            action.clear = true;
        }

        if let Some(verdict) = verdict {
            ui.separator();
            let (strong, soft) = match verdict.passed() {
                true => (theme.ok, theme.ok_soft),
                false => (theme.err, theme.err_soft),
            };
            badge(ui, &verdict.summary(), strong, soft);
        }
    });

    // Anything the drawing could not be squared with the design.
    for problem in problems {
        ui.label(RichText::new(problem).small().color(theme.warn));
    }

    // `run` is pressed here, so a run that produced nothing has to say so here.
    // It used to be reported only into the waveform pane, and the window comes
    // back to this tab afterwards — so the button read as doing nothing at all.
    if let Some(why) = refused.filter(|_| running.is_none()) {
        ui.add_space(6.0);
        crate::views::refusal(ui, why);
        ui.add_space(6.0);
    }

    // Every failure is a column to look at, so it is a button.
    if let Some(verdict) = verdict
        && !verdict.failures.is_empty()
    {
        ui.horizontal_wrapped(|ui| {
            for miss in &verdict.failures {
                let label = format!("× {} @{}", miss.port, miss.cycle);
                let hover =
                    format!("expected {}, got {} at {} ns", miss.expected, miss.got, miss.time_ns);
                if ui.button(label).on_hover_text(hover).clicked() {
                    action.seek = Some(miss.cycle);
                }
            }
        });
    }
    ui.separator();

    if pattern.drive.is_empty() && pattern.expect.is_empty() {
        crate::views::empty_state(
            ui,
            "nothing to draw",
            "This module has no port that is not a clock or a reset, so there is\n\
             nothing a pattern could drive or check.",
        );
        return action;
    }

    egui::ScrollArea::both().id_salt("stim-grid").auto_shrink(false).show(ui, |ui| {
        grid(ui, pane, pattern, verdict, theme);
    });
    action.said = pane.said.take();
    action
}

fn grid(
    ui: &mut Ui,
    pane: &mut StimPane,
    pattern: &mut Pattern,
    verdict: Option<&rtlscope_tb::Verdict>,
    theme: &Theme,
) {
    let rows = pattern.drive.len() + pattern.expect.len() + 1; // the clock has one too
    let size = Vec2::new(NAMES + COLUMN * pattern.cycles as f32 + 8.0, ROW * rows as f32 + 8.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    let left = rect.left() + NAMES;

    // The clock, drawn but not editable: it is what the columns *are*, and the
    // design decided it.
    let clock_top = rect.top() + 2.0;
    painter.text(
        Pos2::new(rect.left() + 4.0, clock_top + ROW / 2.0),
        Align2::LEFT_CENTER,
        &pattern.clock,
        FontId::monospace(11.0),
        theme.muted,
    );
    for column in 0..pattern.cycles {
        let x = left + COLUMN * column as f32;
        // A cycle: high for the first half, low for the second, with the rising
        // edge at the column's left where the drawing says it is.
        let (top, bottom) = (clock_top + 6.0, clock_top + ROW - 8.0);
        painter
            .line_segment([Pos2::new(x, bottom), Pos2::new(x, top)], Stroke::new(1.0, theme.clock));
        painter.line_segment(
            [Pos2::new(x, top), Pos2::new(x + COLUMN / 2.0, top)],
            Stroke::new(1.0, theme.clock),
        );
        painter.line_segment(
            [Pos2::new(x + COLUMN / 2.0, top), Pos2::new(x + COLUMN / 2.0, bottom)],
            Stroke::new(1.0, theme.clock),
        );
        painter.line_segment(
            [Pos2::new(x + COLUMN / 2.0, bottom), Pos2::new(x + COLUMN, bottom)],
            Stroke::new(1.0, theme.clock),
        );
        if column % 5 == 0 {
            painter.text(
                Pos2::new(x + 1.0, clock_top),
                Align2::LEFT_TOP,
                column.to_string(),
                FontId::monospace(8.0),
                theme.muted,
            );
        }
    }

    // Which columns a verdict complained about, so the drawing can point at
    // the same moment the report does.
    let failed: Vec<(String, u64)> = verdict
        .map(|verdict| {
            verdict.failures.iter().map(|miss| (miss.port.clone(), miss.cycle)).collect()
        })
        .unwrap_or_default();

    let mut row = 1usize;
    for expects in [false, true] {
        let lanes = match expects {
            false => &mut pattern.drive,
            true => &mut pattern.expect,
        };
        for (index, lane) in lanes.iter_mut().enumerate() {
            let top = rect.top() + ROW * row as f32 + 2.0;
            row += 1;
            painter.line_segment(
                [Pos2::new(rect.left(), top), Pos2::new(rect.right(), top)],
                Stroke::new(0.5, theme.line),
            );
            lane_row(
                ui,
                &painter,
                pane,
                lane,
                index,
                expects,
                pattern.cycles,
                top,
                left,
                theme,
                &failed,
            );
        }
        // A rule between what is driven and what is expected: they are read the
        // same way but they are not the same claim.
        if !expects && !pattern.expect.is_empty() {
            let at = rect.top() + ROW * row as f32 + 2.0;
            painter.line_segment(
                [Pos2::new(rect.left(), at), Pos2::new(rect.right(), at)],
                Stroke::new(1.5, theme.accent),
            );
        }
    }

    // Last, when no lane is held: a click that opens an entry may have landed
    // on a row this pass had already walked past.
    open_clicked(pane, pattern);
}

/// One port's row: its name, and a cell per column.
#[allow(clippy::too_many_arguments)]
fn lane_row(
    ui: &mut Ui,
    painter: &egui::Painter,
    pane: &mut StimPane,
    lane: &mut Lane,
    index: usize,
    expects: bool,
    cycles: u64,
    top: f32,
    left: f32,
    theme: &Theme,
    failed: &[(String, u64)],
) {
    let middle = top + ROW / 2.0;
    let too_wide = lane.width > rtlscope_tb::pattern::MAX_WIDTH;
    let name = match lane.width {
        1 => lane.port.clone(),
        width => format!("{}[{}:0]", lane.port, width - 1),
    };
    painter.text(
        Pos2::new(left - NAMES + 4.0, middle),
        Align2::LEFT_CENTER,
        &name,
        FontId::monospace(11.0),
        if too_wide { theme.muted } else { theme.ink },
    );
    if too_wide {
        painter.text(
            Pos2::new(left - 6.0, middle),
            Align2::RIGHT_CENTER,
            "too wide",
            FontId::monospace(9.0),
            theme.warn,
        );
        return;
    }

    for column in 0..cycles {
        let at = Rect::from_min_size(
            Pos2::new(left + COLUMN * column as f32, top + 2.0),
            Vec2::new(COLUMN, ROW - 4.0),
        );
        let response = ui.allocate_rect(at, Sense::click_and_drag());
        let held = lane.at(column);

        // The column a verdict complained about, marked where the reader is
        // already looking rather than only in the list above.
        if failed.iter().any(|(port, cycle)| *port == lane.port && *cycle == column) {
            painter.rect_filled(at, 0.0, theme.err_soft);
        } else if response.hovered() {
            painter.rect_filled(at, 0.0, theme.surface_alt);
        }

        draw_cell(painter, at, lane, column, held, expects, theme);
        edit_cell(ui, pane, lane, index, expects, column, &response, held);
    }
}

/// A cell, drawn the way the waveform panel would draw it.
fn draw_cell(
    painter: &egui::Painter,
    at: Rect,
    lane: &Lane,
    column: u64,
    held: Cell,
    expects: bool,
    theme: &Theme,
) {
    let changed = column == 0 || lane.at(column.saturating_sub(1)) != held;
    match held {
        // Nothing is claimed here. Drawn as a faint dotted middle rather than
        // as a zero, because it is not one.
        Cell::DontCare if expects => {
            let y = at.center().y;
            let mut x = at.left() + 2.0;
            while x < at.right() - 2.0 {
                painter.line_segment(
                    [Pos2::new(x, y), Pos2::new(x + 2.0, y)],
                    Stroke::new(1.0, theme.line),
                );
                x += 5.0;
            }
        }
        Cell::DontCare => {
            painter.rect_filled(at.shrink2(Vec2::new(0.0, 4.0)), 0.0, theme.err_soft);
            if changed {
                painter.text(
                    Pos2::new(at.center().x, at.center().y),
                    Align2::CENTER_CENTER,
                    "x",
                    FontId::monospace(10.0),
                    theme.err,
                );
            }
        }
        Cell::Value(value) if lane.width == 1 => {
            let y = match value {
                0 => at.bottom() - 5.0,
                _ => at.top() + 5.0,
            };
            painter.line_segment(
                [Pos2::new(at.left(), y), Pos2::new(at.right(), y)],
                Stroke::new(1.6, theme.wave_line),
            );
            if changed && column > 0 {
                let other = match value {
                    0 => at.top() + 5.0,
                    _ => at.bottom() - 5.0,
                };
                painter.line_segment(
                    [Pos2::new(at.left(), other), Pos2::new(at.left(), y)],
                    Stroke::new(1.6, theme.wave_line),
                );
            }
        }
        Cell::Value(value) => {
            // A bus: a band with the value written where it changes, which is
            // the same shape a waveform gives a bus that is holding.
            let band = at.shrink2(Vec2::new(0.0, 5.0));
            painter.rect_filled(band, 0.0, theme.wave_busy.gamma_multiply(0.5));
            painter.line_segment(
                [band.left_top(), band.right_top()],
                Stroke::new(1.0, theme.wave_line),
            );
            painter.line_segment(
                [band.left_bottom(), band.right_bottom()],
                Stroke::new(1.0, theme.wave_line),
            );
            if changed {
                painter.line_segment(
                    [band.left_top(), band.left_bottom()],
                    Stroke::new(1.0, theme.wave_line),
                );
                painter.text(
                    Pos2::new(band.left() + 2.0, band.center().y),
                    Align2::LEFT_CENTER,
                    format!("{value:x}"),
                    FontId::monospace(9.0),
                    theme.ink,
                );
            }
        }
    }
}

/// What a click on a cell does.
///
/// A one-bit row cycles, because with three states and no modifier that is the
/// whole vocabulary and it needs no explaining. A bus opens an entry, because
/// a number cannot be cycled to.
#[allow(clippy::too_many_arguments)]
fn edit_cell(
    ui: &mut Ui,
    pane: &mut StimPane,
    lane: &mut Lane,
    index: usize,
    expects: bool,
    column: u64,
    response: &egui::Response,
    held: Cell,
) {
    let one_bit = lane.width == 1;

    if one_bit {
        if response.drag_started() {
            let next = cycle_through(held, expects);
            pane.painting = Some(next);
            lane.set(column, next);
        } else if response.dragged() {
            // The whole run gets what the first cell got, rather than each
            // cell flipping as the pointer crosses it.
            if let Some(painting) = pane.painting {
                lane.set(column, painting);
            }
        } else if response.clicked() {
            lane.set(column, cycle_through(held, expects));
        }
        if response.drag_stopped() {
            pane.painting = None;
        }
        return;
    }

    if response.clicked() {
        pane.wants = Some((expects, index, column));
    }
    // Right-click puts a bus back to not-drawn, which for an expectation is
    // "say nothing" and for an input is "drive x".
    if response.secondary_clicked() {
        lane.set(column, Cell::DontCare);
    }

    if pane.editing == Some((expects, index, column)) {
        let entry = ui.put(
            response.rect,
            egui::TextEdit::singleline(&mut pane.entry)
                .font(FontId::monospace(9.0))
                .margin(egui::Margin::same(1)),
        );
        // Once, when the entry opens. Asking every frame meant nothing ever
        // lost focus, and losing focus is what takes the value.
        if std::mem::take(&mut pane.taking) {
            entry.request_focus();
        }
        if entry.lost_focus() {
            let typed = std::mem::take(&mut pane.entry);
            pane.editing = None;
            pane.said = typed_into(lane, column, &typed);
        }
    }
}

/// Puts what was typed into a cell, or says why it did not.
///
/// Empty means "nothing here": don't-care, not zero. A row nobody drew into
/// claims nothing, and typing a value out again has to be able to say that.
///
/// A value too big for the port is **refused**, not masked. The VCD writes the
/// low bits of whatever it is given and cocotb hands the number straight to the
/// port, so a quietly narrowed `102f` would be drawn as one value, driven as
/// another and reported as a third — the same reason a port wider than
/// [`rtlscope_tb::pattern::MAX_WIDTH`] is left alone by name rather than halved.
fn typed_into(lane: &mut Lane, column: u64, typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        lane.set(column, Cell::DontCare);
        return None;
    }
    let Ok(cell) = Cell::try_from(typed.to_string()) else {
        return Some(format!("`{typed}` is not a hexadecimal value or `x`"));
    };
    if let Cell::Value(value) = cell
        && lane.width < u64::BITS
        && value >> lane.width != 0
    {
        return Some(format!(
            "{value:x} does not fit `{}`, which is {} bit(s) wide",
            lane.port, lane.width
        ));
    }
    lane.set(column, cell);
    None
}

/// The lane a cell sits in.
fn lane_at(pattern: &mut Pattern, (expects, index, _): At) -> Option<&mut Lane> {
    let lanes = match expects {
        true => &mut pattern.expect,
        false => &mut pattern.drive,
    };
    lanes.get_mut(index)
}

/// Opens the entry on the cell that was clicked, first taking whatever was
/// typed into the one that was open.
///
/// After the rows rather than during them: the two cells are walked in the
/// order they sit in, so the open one is as likely to come after the clicked
/// one as before, and doing this where the click lands loses the edit whenever
/// it comes after.
fn open_clicked(pane: &mut StimPane, pattern: &mut Pattern) {
    let Some(wanted) = pane.wants.take() else { return };
    if pane.editing == Some(wanted) {
        return;
    }
    // Only when the row itself did not already take it on losing focus, which
    // is the ordinary way out and happens earlier in the same frame.
    if let Some(open) = pane.editing.take() {
        let typed = std::mem::take(&mut pane.entry);
        if let Some(lane) = lane_at(pattern, open) {
            pane.said = typed_into(lane, open.2, &typed);
        }
    }

    pane.entry = match lane_at(pattern, wanted).map(|lane| lane.at(wanted.2)) {
        Some(Cell::Value(value)) => format!("{value:x}"),
        _ => String::new(),
    };
    pane.editing = Some(wanted);
    pane.taking = true;
}

/// The next state a one-bit cell takes when it is clicked.
///
/// A row that drives starts from a defined value, so `x` comes last; a row that
/// expects starts from saying nothing, so that is where it returns to.
fn cycle_through(held: Cell, expects: bool) -> Cell {
    match (held, expects) {
        (Cell::Value(0), _) => Cell::Value(1),
        (Cell::Value(_), _) => Cell::DontCare,
        (Cell::DontCare, _) => Cell::Value(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three states and one gesture: a click has to reach all of them and come
    /// back, or some of the vocabulary is unreachable without a manual.
    #[test]
    fn clicking_a_one_bit_cell_reaches_every_state_and_returns() {
        let mut held = Cell::Value(0);
        let mut seen = vec![held];
        for _ in 0..3 {
            held = cycle_through(held, false);
            seen.push(held);
        }
        assert_eq!(seen, [Cell::Value(0), Cell::Value(1), Cell::DontCare, Cell::Value(0)]);
    }

    fn a_drawing() -> Pattern {
        Pattern {
            module: "trace_demo".to_string(),
            clock: "clk".to_string(),
            period_ns: 10,
            cycles: 8,
            drive: vec![Lane::driven("in_data", 8), Lane::driven("mode", 2)],
            expect: vec![Lane::expected("out_data", 8)],
        }
    }

    /// A bus cannot be cycled to, so it opens an entry — and what is typed
    /// there has to arrive in the lane. It did not: the entry asked for focus
    /// on every frame, so egui never counted focus as lost, and losing focus
    /// was the only thing that took the value.
    #[test]
    fn what_is_typed_into_a_bus_reaches_the_lane() {
        let mut pattern = a_drawing();
        let mut pane = StimPane { wants: Some((false, 0, 3)), ..StimPane::default() };
        open_clicked(&mut pane, &mut pattern);
        assert_eq!(pane.editing, Some((false, 0, 3)));
        assert!(pane.taking, "the entry is handed the keyboard once, when it opens");

        let lane = lane_at(&mut pattern, (false, 0, 3)).expect("the row that was clicked");
        typed_into(lane, 3, "2f");
        assert_eq!(pattern.drive[0].at(3), Cell::Value(0x2f));
        assert_eq!(pattern.drive[0].at(2), Cell::Value(0), "and only that column moved");
    }

    /// The cells are walked in the order they sit in, so the one being edited
    /// is as likely to come after the one just clicked as before. Opening the
    /// new entry where the click landed threw the old edit away whenever it
    /// did, which is why the opening is left until every row has been walked.
    #[test]
    fn clicking_another_cell_keeps_what_was_typed_into_the_last_one() {
        let mut pattern = a_drawing();
        let mut pane = StimPane { wants: Some((false, 0, 5)), ..StimPane::default() };
        open_clicked(&mut pane, &mut pattern);
        pane.entry = "ab".to_string();

        // A cell earlier in the same pass, which is the case that lost the edit.
        pane.wants = Some((false, 0, 1));
        open_clicked(&mut pane, &mut pattern);

        assert_eq!(pattern.drive[0].at(5), Cell::Value(0xab), "the first edit was taken");
        assert_eq!(pane.editing, Some((false, 0, 1)), "and the second entry is open");
        assert_eq!(pane.entry, "0", "showing what that cell holds, which a drive row starts at");
    }

    /// Clicking where the caret already is must not be a commit-and-reopen:
    /// that would replace what is half typed with what the cell still holds.
    #[test]
    fn clicking_the_cell_already_open_leaves_what_is_being_typed_alone() {
        let mut pattern = a_drawing();
        let mut pane = StimPane { wants: Some((false, 0, 5)), ..StimPane::default() };
        open_clicked(&mut pane, &mut pattern);
        pane.entry = "ff".to_string();

        pane.wants = Some((false, 0, 5));
        open_clicked(&mut pane, &mut pattern);

        assert_eq!(pane.entry, "ff", "still being typed");
        assert_eq!(pattern.drive[0].at(5), Cell::Value(0), "and not written yet");
    }

    /// Emptying a bus cell says "nothing here" rather than "zero" — the same
    /// distinction the whole expect half of the drawing rests on.
    #[test]
    fn an_emptied_bus_cell_goes_back_to_not_drawn() {
        let mut lane = Lane::driven("in_data", 8);
        lane.set(2, Cell::Value(0x11));
        typed_into(&mut lane, 2, "  ");
        assert_eq!(lane.at(2), Cell::DontCare);
    }

    /// A typo leaves the cell as it was. Writing zero for it would be inventing
    /// a value nobody drew.
    #[test]
    fn something_that_is_not_a_number_leaves_the_cell_as_it_was() {
        let mut lane = Lane::driven("in_data", 8);
        lane.set(2, Cell::Value(0x11));

        let said = typed_into(&mut lane, 2, "zz").expect("a reason");
        assert!(said.contains("hexadecimal"), "{said}");
        assert_eq!(lane.at(2), Cell::Value(0x11), "and the cell is untouched");

        assert_eq!(typed_into(&mut lane, 2, "0x3c"), None);
        assert_eq!(lane.at(2), Cell::Value(0x3c), "`0x` in front is taken, not refused");
    }

    /// Measured by typing into the window: an 8-bit port took `102f` and drew
    /// it. The VCD writes the low bits of whatever it is handed and cocotb
    /// gives the number straight to the port, so a value that was quietly
    /// narrowed would be drawn as one thing, driven as another, and reported
    /// as a third.
    #[test]
    fn a_value_too_big_for_the_port_is_refused_rather_than_narrowed() {
        let mut lane = Lane::driven("in_data", 8);
        lane.set(2, Cell::Value(0x11));

        let said = typed_into(&mut lane, 2, "102f").expect("a reason");
        assert!(said.contains("in_data"), "which port:\n{said}");
        assert!(said.contains("8 bit(s)"), "and how wide it is:\n{said}");
        assert_eq!(lane.at(2), Cell::Value(0x11), "the cell keeps what it had");

        assert_eq!(typed_into(&mut lane, 2, "ff"), None, "and what does fit still goes in");
        assert_eq!(lane.at(2), Cell::Value(0xff));
    }

    /// The widest a drawn value may be. Shifting by the width of the type is
    /// undefined, so the check has to step around exactly this case.
    #[test]
    fn a_sixty_four_bit_port_takes_the_largest_value_there_is() {
        let mut lane = Lane::driven("wide", 64);
        assert_eq!(typed_into(&mut lane, 0, "ffffffffffffffff"), None);
        assert_eq!(lane.at(0), Cell::Value(u64::MAX));
    }

    /// A one-bit row is clicked rather than typed into, but the same guard
    /// covers it — and `1` is the only value it can hold besides zero.
    #[test]
    fn a_one_bit_row_will_not_take_a_two() {
        let mut lane = Lane::driven("in_valid", 1);
        assert!(typed_into(&mut lane, 0, "2").is_some());
        assert_eq!(lane.at(0), Cell::Value(0), "still what a drive row starts at");
    }
}
