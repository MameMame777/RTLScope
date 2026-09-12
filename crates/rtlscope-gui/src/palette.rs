//! Finding a name without knowing where it lives.
//!
//! The hierarchy tree is how a reader walks a design they already understand.
//! It is a poor way to answer "where is `frame_lines_runtime`", which is the
//! question somebody has when they arrive at a design of thirty-five modules
//! with a bug report and a signal name. That walk is several clicks through
//! names they are not looking for.
//!
//! So: a name, and a list that narrows as it is typed. Nothing here searches
//! the *source* — RTLScope already knows every module and every signal by name,
//! and a match against that list can go somewhere, where a match in a file can
//! only be shown.

use egui::{Key, RichText, Ui};
use rtlscope_analyse::flat::{Flattened, SignalId};
use rtlscope_ir::{Design, ModuleId};

/// What the palette is showing, and what has been typed into it.
#[derive(Debug, Default)]
pub struct Palette {
    pub open: bool,
    pub query: String,
    /// Which row the arrow keys are on.
    pub chosen: usize,
    /// True on the frame it opens, so the text field can take focus once.
    pub just_opened: bool,
}

impl Palette {
    pub fn show(&mut self) {
        self.open = true;
        self.just_opened = true;
        self.chosen = 0;
    }
}

/// One thing worth jumping to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Module(ModuleId),
    Signal(SignalId),
}

/// A row of the list.
pub struct Hit {
    pub what: Found,
    pub name: String,
    /// What kind of thing it is, said beside the name.
    pub kind: &'static str,
    score: u32,
}

/// How many rows are offered.
///
/// Enough that a vague query still shows the thing wanted; few enough that the
/// list is read rather than scrolled. A query matching more than this is a
/// query worth narrowing.
const SHOWN: usize = 40;

/// How well a name answers a query, if it answers it at all.
///
/// A subsequence match — the letters in order, not necessarily adjacent — so
/// `frl` finds `frame_lines`. Scored so that the obvious answer comes first:
///
/// - a match in the last segment beats one in the path, because a hierarchical
///   name is a long prefix and a short distinguishing tail, and the tail is
///   what somebody types;
/// - letters found together beat letters found scattered;
/// - a short name beats a long one holding the same letters, since the extra
///   letters are ones the reader did not ask for.
///
/// Written here rather than pulled in, because a fuzzy matcher is thirty lines
/// and a dependency is forever.
pub fn score(query: &str, name: &str) -> Option<u32> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let lower = name.to_ascii_lowercase();
    let haystack: Vec<char> = lower.chars().collect();
    let tail_from = lower.rfind('.').map(|at| lower[..at].chars().count() + 1).unwrap_or(0);

    let mut points = 0u32;
    let mut at = 0usize;
    let mut previous: Option<usize> = None;
    for wanted in query.chars() {
        let found = haystack[at..].iter().position(|c| *c == wanted)? + at;
        // Adjacent to the last letter: the reader typed a run of the name
        // rather than letters that happen to appear in it.
        if previous == Some(found.wrapping_sub(1)) {
            points += 8;
        }
        // In the last segment, which is the part somebody knows by heart.
        if found >= tail_from {
            points += 6;
        }
        // At a word boundary — the start, or after `_` or `.`.
        if found == 0 || matches!(haystack.get(found - 1), Some('_' | '.')) {
            points += 4;
        }
        previous = Some(found);
        at = found + 1;
    }
    // Shorter is better, all else equal: `clk` should beat `clk_divider_reset`
    // for the query `clk`.
    Some(points + 60u32.saturating_sub(haystack.len() as u32))
}

/// Everything in the design a query might mean, best first.
pub fn hits(design: &Design, flat: &Flattened, query: &str) -> Vec<Hit> {
    let mut found: Vec<Hit> = Vec::new();

    for (id, module) in design.modules.iter_enumerated() {
        if let Some(score) = score(query, &module.name) {
            found.push(Hit {
                what: Found::Module(id),
                name: module.name.clone(),
                kind: "module",
                // Modules first among equals: there are far fewer of them, and
                // somebody typing a module's name means the module.
                score: score + 10,
            });
        }
    }

    // One row per signal, under the name it reads best by. `all_names` offers
    // every alias a signal wears across boundaries, and listing a wire once per
    // module it passes through would bury the design in its own hierarchy.
    let mut seen = std::collections::HashSet::new();
    for (name, signal, ..) in flat.all_names(design) {
        if !seen.insert(signal) {
            continue;
        }
        if let Some(score) = score(query, &name) {
            found.push(Hit { what: Found::Signal(signal), name, kind: "signal", score });
        }
    }

    // By score, then by name, so equal answers come out in the same order twice.
    found.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    found.truncate(SHOWN);
    found
}

/// The palette itself. Returns what was chosen, if anything was.
pub fn show(ui: &Ui, palette: &mut Palette, design: &Design, flat: &Flattened) -> Option<Found> {
    let theme = crate::theme::Theme::of(ui);
    let mut chosen = None;

    let screen = ui.ctx().input(|input| input.content_rect());
    let width = (screen.width() * 0.5).clamp(360.0, 720.0);
    let area = egui::Area::new(ui.id().with("palette"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(screen.center().x - width / 2.0, screen.top() + 80.0));

    area.show(ui.ctx(), |ui| {
        egui::Frame::popup(ui.style()).inner_margin(10).show(ui, |ui| {
            ui.set_width(width);

            let entry = ui.add(
                egui::TextEdit::singleline(&mut palette.query)
                    .desired_width(width)
                    .hint_text("a module or signal name")
                    .font(egui::TextStyle::Monospace),
            );
            // Focus taken once, not every frame: asking for it repeatedly puts
            // the caret back at the start of what is being typed.
            if std::mem::take(&mut palette.just_opened) {
                entry.request_focus();
            }

            let found = hits(design, flat, &palette.query);
            if found.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("nothing by that name in this design").weak());
                return;
            }
            palette.chosen = palette.chosen.min(found.len() - 1);

            // The keys are read here rather than globally, because the palette
            // holds focus while it is open and everything else in the window is
            // already written to stay quiet when something does.
            let (up, down, enter) = ui.input(|input| {
                (
                    input.key_pressed(Key::ArrowUp),
                    input.key_pressed(Key::ArrowDown),
                    input.key_pressed(Key::Enter),
                )
            });
            if down {
                palette.chosen = (palette.chosen + 1).min(found.len() - 1);
            }
            if up {
                palette.chosen = palette.chosen.saturating_sub(1);
            }

            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for (index, hit) in found.iter().enumerate() {
                    let picked = index == palette.chosen;
                    // The row is claimed before anything is drawn in it, so the
                    // highlight goes behind the text rather than over it and
                    // spans the whole width rather than the words' own.
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 20.0),
                        egui::Sense::click(),
                    );
                    if picked {
                        ui.painter().rect_filled(rect, 3.0, theme.accent_soft);
                    }
                    let painter = ui.painter();
                    painter.text(
                        rect.left_center() + egui::vec2(6.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        hit.kind,
                        egui::FontId::proportional(10.0),
                        match hit.kind {
                            "module" => theme.accent,
                            _ => theme.muted,
                        },
                    );
                    painter.text(
                        rect.left_center() + egui::vec2(52.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        &hit.name,
                        egui::FontId::monospace(12.0),
                        if picked { theme.accent } else { theme.ink },
                    );
                    if response.clicked() {
                        chosen = Some(hit.what.clone());
                    }
                }
            });

            if enter {
                chosen = Some(found[palette.chosen].what.clone());
            }
        });
    });

    if ui.input(|input| input.key_pressed(Key::Escape)) || chosen.is_some() {
        palette.open = false;
        palette.query.clear();
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tail is what somebody types. A hierarchical name is a long prefix
    /// and a short distinguishing end, and matching the prefix is matching the
    /// part every row shares.
    #[test]
    fn a_match_in_the_last_segment_beats_one_in_the_path() {
        let tail = score("core", "tb.dut.core_clk").expect("matches");
        let path = score("core", "tb.core_thing.u_other").expect("matches");
        assert!(tail > path, "tail {tail} should beat path {path}");
    }

    /// Letters found together are a run of the name; letters found scattered
    /// are a coincidence.
    #[test]
    fn letters_found_together_beat_letters_found_scattered() {
        let together = score("clk", "clk").expect("matches");
        let scattered = score("clk", "c_long_kind").expect("matches");
        assert!(together > scattered, "{together} should beat {scattered}");
    }

    /// The short name is the one meant. Otherwise `clk` offers every signal
    /// with a clock in its name before the clock itself.
    #[test]
    fn the_shorter_of_two_names_holding_the_query_comes_first() {
        let short = score("clk", "clk").expect("matches");
        let long = score("clk", "clk_divider_reset_sync").expect("matches");
        assert!(short > long, "{short} should beat {long}");
    }

    /// Letters in order, not necessarily adjacent: `frl` should find
    /// `frame_lines` without anybody typing the underscore.
    #[test]
    fn the_letters_only_have_to_be_in_order() {
        assert!(score("frl", "frame_lines").is_some());
        assert!(score("lfr", "frame_lines").is_none(), "but the order is the query's");
    }

    /// An empty query is not a failed match — it is the whole list, which is
    /// what a palette shows before anything is typed.
    #[test]
    fn an_empty_query_matches_everything() {
        assert_eq!(score("", "anything"), Some(0));
        assert_eq!(score("   ", "anything"), Some(0));
    }
}
