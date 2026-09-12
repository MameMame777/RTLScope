//! Following one thing through the pipeline, the way Konata does.
//!
//! [`crate::stages`] answers "what was stage 2 doing on cycle 40". This answers
//! the question a reader actually has, which is the transpose of it: *where did
//! this beat go*. One row per thing that entered the pipe, one column per
//! cycle, and the cell says which stage it was in — so a beat's life is a
//! diagonal, a stall is a horizontal run, and a bubble is a gap between two
//! diagonals. Nothing in the dump says any of this; it is reconstructed from
//! the occupancy grid, and that reconstruction is the whole of this module.
//!
//! **It is an inference, and a narrow one.** The rule is that a stage which is
//! busy this cycle holds whatever the stage above it held last cycle — which is
//! true of a pipeline that advances in lockstep, and false of one that
//! reorders, forwards, or lets stages run at different rates. Everything the
//! rule cannot account for is counted into [`TokenFlow::problems`] rather than
//! being smoothed over: a token that appeared in the middle of the pipe with
//! nothing feeding it, one that vanished before the end. A picture of a
//! pipeline this cannot follow should look wrong, not plausible.
//!
//! Two more things are deliberately *not* problems. A token already in flight
//! when the window opens has no visible birth, and one still in flight when it
//! closes has no visible retirement; both are facts about the window, not about
//! the design, so they are left alone.

use serde::Serialize;

use crate::stages::{Cell, StageView};

/// How many tokens one window may hold before this gives up.
///
/// A window is capped at [`crate::stages::MAX_WINDOW`] cycles, and a pipeline
/// that starts something every cycle produces about that many tokens; past this
/// there is more than one token per row of pixels on any screen, and the
/// picture has stopped being one.
pub const MAX_TOKENS: usize = 4096;

/// Where one token was on one cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TokenStep {
    /// Absolute, so it lines up with the waveform's cursor and with
    /// `rtlscope stages`.
    pub cycle: usize,
    pub stage: usize,
    /// It was here last cycle too: the pipe did not advance.
    pub stalled: bool,
    /// Some bit of this stage was `x` or `z` — it is here, but what it holds is
    /// not known.
    pub unknown: bool,
}

/// One thing that went through the pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Token {
    pub id: usize,
    /// What the stage it entered at was carrying, when a payload was chosen —
    /// otherwise `#id`, which at least distinguishes two rows.
    pub label: String,
    /// In cycle order, one per cycle it was somewhere.
    pub steps: Vec<TokenStep>,
    /// It first appeared partway down the pipe with nothing above it to have
    /// come from. Either the rule does not fit this design or the dump is
    /// missing a stage, and both are worth seeing.
    pub appeared_mid_pipe: bool,
    /// It stopped before reaching the last stage, and not at the end of the
    /// window: it was dropped, or the pipe does not advance the way this
    /// assumes.
    pub vanished: bool,
}

impl Token {
    pub fn first_cycle(&self) -> usize {
        self.steps.first().map_or(0, |step| step.cycle)
    }

    pub fn last_cycle(&self) -> usize {
        self.steps.last().map_or(0, |step| step.cycle)
    }

    /// Where it was on a cycle, if it was anywhere.
    pub fn at(&self, cycle: usize) -> Option<&TokenStep> {
        // The steps are contiguous in cycle order, so this is an index rather
        // than a search.
        let first = self.steps.first()?.cycle;
        let offset = cycle.checked_sub(first)?;
        self.steps.get(offset).filter(|step| step.cycle == cycle)
    }

    /// How many of its cycles it spent not moving.
    pub fn stalls(&self) -> usize {
        self.steps.iter().filter(|step| step.stalled).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenFlow {
    pub clock: String,
    /// How deep the pipeline is, so a drawing knows how many colours it needs.
    pub depth: usize,
    /// The window these tokens were followed through.
    pub first: usize,
    pub len: usize,
    /// Oldest first.
    pub tokens: Vec<Token>,
    /// Everything the following rule could not account for.
    pub problems: Vec<String>,
}

impl TokenFlow {
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The last cycle in the window, for a ruler.
    pub fn last(&self) -> usize {
        self.first + self.len.saturating_sub(1)
    }
}

/// Follows every token through a window of stage occupancy.
pub fn tokens(view: &StageView) -> TokenFlow {
    let mut flow = TokenFlow {
        clock: view.clock.clone(),
        depth: view.depth,
        first: view.first,
        len: view.len(),
        tokens: Vec::new(),
        problems: Vec::new(),
    };

    // The rows come from the analysis in stage order, but nothing promises it,
    // and the whole rule depends on "the stage above" being the one before.
    let mut rows: Vec<&crate::stages::StageRow> = view.rows.iter().collect();
    rows.sort_by_key(|row| row.stage);
    if rows.is_empty() || view.is_empty() {
        return flow;
    }

    // Who is in each stage, as an index into `flow.tokens`.
    let mut held: Vec<Option<usize>> = vec![None; rows.len()];
    let mut capped = false;
    let mut mid_pipe = 0usize;

    for column in 0..view.len() {
        let cycle = view.first + column;
        let mut next: Vec<Option<usize>> = vec![None; rows.len()];

        // Deepest first, so a stage takes from the one above before that one
        // gives itself away: without it a whole pipe advancing in lockstep
        // would have every stage claim the same token.
        for depth in (0..rows.len()).rev() {
            let row = rows[depth];
            let cell = row.cells.get(column).copied().unwrap_or(Cell::Blank);
            if matches!(cell, Cell::Blank | Cell::Idle) {
                continue;
            }

            // Taken out of `held` rather than read from it: two stages must
            // never come away holding the same token — a stage that stalls and
            // the one below it advancing would otherwise both claim it — and
            // whatever is left in `held` at the end of the column is then
            // exactly what left the pipe.
            let taken = match cell {
                // It advanced: whatever was above it is now here.
                Cell::Busy => above(depth, &mut held),
                // It did not advance: whatever was here is still here.
                Cell::Held => held[depth].take(),
                // Undriven. Whether it advanced cannot be told, so this reads
                // it as an advance — the commoner case, and the one that keeps
                // a diagonal unbroken — and falls back to standing still. The
                // step is marked either way, so nothing is taken on trust.
                Cell::Unknown => above(depth, &mut held).or_else(|| held[depth].take()),
                Cell::Blank | Cell::Idle => None,
            };

            let token = match taken {
                Some(token) => token,
                // Nothing to have come from. At the top of the pipe on any
                // cycle, or anywhere on the window's first column, that is
                // simply where the window starts; deeper in, it is the rule
                // failing, and the token says so.
                None => {
                    if flow.tokens.len() >= MAX_TOKENS {
                        capped = true;
                        continue;
                    }
                    let born_blind = depth > 0 && column > 0;
                    if born_blind {
                        mid_pipe += 1;
                    }
                    let id = flow.tokens.len();
                    flow.tokens.push(Token {
                        id,
                        label: label(row, column, id),
                        steps: Vec::new(),
                        appeared_mid_pipe: born_blind,
                        vanished: false,
                    });
                    id
                }
            };

            next[depth] = Some(token);
            flow.tokens[token].steps.push(TokenStep {
                cycle,
                stage: row.stage,
                stalled: matches!(cell, Cell::Held),
                unknown: matches!(cell, Cell::Unknown),
            });
        }

        // Anything still in `held` was not claimed, so it has left the pipe.
        // From the last stage that is retirement; from anywhere else it is a
        // token that went missing, which the rule cannot explain.
        for (depth, left) in held.iter().enumerate() {
            if let Some(token) = *left
                && depth + 1 < rows.len()
            {
                flow.tokens[token].vanished = true;
            }
        }
        held = next;
    }

    // A token still in flight when the window closes has not vanished; it is
    // only out of view, and saying otherwise would flag every window's edge.
    let edge = flow.last();
    for token in &mut flow.tokens {
        if token.last_cycle() == edge {
            token.vanished = false;
        }
    }
    let vanished = flow.tokens.iter().filter(|token| token.vanished).count();

    if capped {
        flow.problems.push(format!(
            "more than {MAX_TOKENS} token(s) pass through this window and the rest are not \
             followed; ask for fewer cycles"
        ));
    }
    if mid_pipe > 0 {
        flow.problems.push(format!(
            "{mid_pipe} token(s) first appear partway down the pipe with nothing above them to \
             have come from — either this pipeline does not advance in lockstep, or a stage \
             between them is missing from the dump"
        ));
    }
    if vanished > 0 {
        flow.problems.push(format!(
            "{vanished} token(s) stop before the last stage and before the end of the window: \
             they were dropped, or they left by a path this does not follow"
        ));
    }
    flow
}

/// Takes whatever the stage above was holding, leaving it held by nobody.
fn above(depth: usize, held: &mut [Option<usize>]) -> Option<usize> {
    held[depth.checked_sub(1)?].take()
}

/// What to call a token, from what the stage it entered was carrying.
fn label(row: &crate::stages::StageRow, column: usize, id: usize) -> String {
    match row.values.get(column) {
        Some(value) if !value.is_empty() => value.clone(),
        _ => format!("#{id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::{Basis, StageRow};

    /// A view built straight from glyphs, so a test reads as the picture it is
    /// about: `#` busy, `~` held, `.` idle, `?` undriven, ` ` blank.
    fn view(rows: &[&str]) -> StageView {
        let len = rows.first().map_or(0, |row| row.len());
        StageView {
            clock: "clk".into(),
            clock_path: "tb.clk".into(),
            depth: rows.len(),
            cycles: len,
            first: 0,
            times: (0..len as u64).map(|t| t * 10).collect(),
            rows: rows
                .iter()
                .enumerate()
                .map(|(stage, glyphs)| StageRow {
                    stage,
                    registers: 1,
                    matched: 1,
                    basis: Basis::Valid { signal: format!("v{stage}") },
                    payload: None,
                    cells: glyphs
                        .chars()
                        .map(|glyph| match glyph {
                            '#' => Cell::Busy,
                            '~' => Cell::Held,
                            '.' => Cell::Idle,
                            '?' => Cell::Unknown,
                            _ => Cell::Blank,
                        })
                        .collect(),
                    values: Vec::new(),
                    occupied: 0,
                    held: 0,
                })
                .collect(),
            problems: Vec::new(),
        }
    }

    /// The stages a token visited, as `cycle:stage` pairs.
    fn trail(token: &Token) -> Vec<(usize, usize)> {
        token.steps.iter().map(|step| (step.cycle, step.stage)).collect()
    }

    /// The picture a pipeline is explained with: one beat, going down.
    #[test]
    fn a_beat_walks_one_stage_per_cycle() {
        let flow = tokens(&view(&["#..", ".#.", "..#"]));

        assert_eq!(flow.tokens.len(), 1, "{:#?}", flow.tokens);
        assert_eq!(trail(&flow.tokens[0]), [(0, 0), (1, 1), (2, 2)]);
        assert!(flow.problems.is_empty(), "{:?}", flow.problems);
        assert!(!flow.tokens[0].appeared_mid_pipe);
        assert!(!flow.tokens[0].vanished);
    }

    /// Four in, four out, one behind the other — and each is its own row.
    #[test]
    fn a_burst_becomes_one_token_per_beat() {
        let flow = tokens(&view(&[
            "####...", //
            ".####..", "..####.",
        ]));

        assert_eq!(flow.tokens.len(), 4, "{:#?}", flow.tokens);
        assert_eq!(trail(&flow.tokens[0]), [(0, 0), (1, 1), (2, 2)]);
        assert_eq!(trail(&flow.tokens[3]), [(3, 0), (4, 1), (5, 2)]);
        assert!(flow.problems.is_empty(), "{:?}", flow.problems);
    }

    /// A gap in the input is a gap between tokens, not one long one.
    #[test]
    fn a_bubble_separates_two_tokens() {
        let flow = tokens(&view(&["#.#.", ".#.#", "..#."]));

        assert_eq!(flow.tokens.len(), 2, "{:#?}", flow.tokens);
        assert_eq!(trail(&flow.tokens[0]), [(0, 0), (1, 1), (2, 2)]);
        assert_eq!(trail(&flow.tokens[1]), [(2, 0), (3, 1)]);
        assert!(flow.problems.is_empty(), "{:?}", flow.problems);
    }

    /// A stall is the same token in the same stage twice, and it says so — that
    /// is the thing a reader opens this view to find.
    #[test]
    fn a_stall_is_the_same_token_standing_still() {
        // Arrival is `#` even into a stage that then stalls: a stage is only
        // `Held` once it is repeating what it already had.
        let flow = tokens(&view(&["#...", ".#~.", "...#"]));

        assert_eq!(flow.tokens.len(), 1, "{:#?}", flow.tokens);
        let token = &flow.tokens[0];
        assert_eq!(trail(token), [(0, 0), (1, 1), (2, 1), (3, 2)]);
        assert_eq!(token.stalls(), 1, "only the repeat counts, not the arrival");
        assert!(token.steps[2].stalled);
        assert!(!token.steps[3].stalled);
    }

    /// A stage busy with nothing above it cannot be explained by the rule, and
    /// a picture that hid that would be a guess wearing a diagram's clothes.
    #[test]
    fn a_token_that_appears_mid_pipe_is_flagged_and_counted() {
        let flow = tokens(&view(&["...", "..#", "..."]));

        assert_eq!(flow.tokens.len(), 1);
        assert!(flow.tokens[0].appeared_mid_pipe);
        assert!(
            flow.problems.iter().any(|problem| problem.contains("partway down")),
            "{:?}",
            flow.problems
        );
    }

    /// Whatever is already in flight when the window opens has no visible
    /// birth. That is a fact about the window, so it is not held against it.
    #[test]
    fn the_windows_own_edges_are_not_reported_as_faults() {
        // Column 0 finds a token in the middle stage, and the last column
        // leaves one in flight.
        let flow = tokens(&view(&["..#", "#..", ".#."]));

        assert!(!flow.tokens[0].appeared_mid_pipe, "born at the left edge: {:#?}", flow.tokens[0]);
        assert!(flow.tokens.iter().all(|token| !token.vanished), "{:#?}", flow.tokens);
        assert!(flow.problems.is_empty(), "{:?}", flow.problems);
    }

    /// One that stops in the middle of the pipe, in the middle of the window,
    /// was dropped — and that is exactly what this view is for seeing.
    #[test]
    fn a_token_that_stops_early_is_reported() {
        let flow = tokens(&view(&["#...", ".#..", "...."]));

        assert_eq!(flow.tokens.len(), 1);
        assert!(flow.tokens[0].vanished, "{:#?}", flow.tokens[0]);
        assert!(
            flow.problems.iter().any(|problem| problem.contains("stop before the last stage")),
            "{:?}",
            flow.problems
        );
    }

    /// An undriven stage still holds the token — it is there, and what it has
    /// is unknown. Reading it as empty would lose the token entirely.
    #[test]
    fn an_undriven_stage_keeps_the_token_and_marks_it() {
        let flow = tokens(&view(&["#..", ".?.", "..#"]));

        assert_eq!(flow.tokens.len(), 1, "{:#?}", flow.tokens);
        // Read as an advance, so the diagonal survives — and marked, so the
        // cell that cannot be trusted says so.
        assert_eq!(trail(&flow.tokens[0]), [(0, 0), (1, 1), (2, 2)]);
        assert!(flow.tokens[0].steps[1].unknown);
        assert!(!flow.tokens[0].steps[0].unknown);
    }

    #[test]
    fn an_empty_window_is_not_a_panic() {
        assert!(tokens(&view(&[])).is_empty());
        assert!(tokens(&view(&["", "", ""])).is_empty());
        assert!(tokens(&view(&["...", "...", "..."])).is_empty());
    }
}
