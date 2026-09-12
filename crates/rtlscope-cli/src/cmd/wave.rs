//! Printing what a dump holds and what a decoder made of it.
//!
//! Both reports lead with the answer and keep the evidence underneath: how many
//! frames, how much stalling, which registers were written — then the list, then
//! what could not be accounted for. Annotations are left to `--json`; they are
//! for drawing, not for reading.

use std::fmt::Write as _;

use rtlscope_wave::bind::Suggestion;
use rtlscope_wave::compare::Comparison;
use rtlscope_wave::decode::{DecodeReport, Level};
use rtlscope_wave::matching::MatchReport;
use rtlscope_wave::stages::{Basis, StageView};

/// What is in a dump, and how much of it the design accounts for.
pub fn info_text(report: &MatchReport, dump_vars: usize) -> String {
    let mut out = String::new();

    let scope = if report.prefix.is_empty() { "(the top)" } else { report.prefix.as_str() };
    let _ = writeln!(
        out,
        "{} of {} signal(s) matched, under `{scope}`",
        report.matched.len(),
        report.prefix_score.1
    );
    if report.prefix_score.0 < report.prefix_score.1 {
        let _ = writeln!(
            out,
            "  The scope was inferred by counting: `{scope}` lined up with {} of the design's \
             {} names.",
            report.prefix_score.0, report.prefix_score.1
        );
        out.push_str("  Pass --prefix to name it yourself if that is the wrong one.\n");
    }
    if report.synthesised > 0 {
        let _ = writeln!(
            out,
            "  {} net(s) exist only in the IR — an inlined function's locals, the wire behind \
             an expression in a connection — and have no counterpart in any dump.",
            report.synthesised
        );
    }

    if !report.unmatched_ir.is_empty() {
        let _ = writeln!(out, "\n{} net(s) the dump does not have", report.unmatched_ir.len());
        for missing in report.unmatched_ir.iter().take(20) {
            let _ = writeln!(out, "  {}  — {}", missing.ir_name, missing.reason);
        }
        if report.unmatched_ir.len() > 20 {
            let _ = writeln!(out, "  ... and {} more", report.unmatched_ir.len() - 20);
        }
    }

    let extra = report.unmatched_dump.len();
    if extra > 0 {
        let _ = writeln!(
            out,
            "\n{extra} variable(s) in the dump belong to nothing in the design — the \
             testbench's own, mostly. {dump_vars} variable(s) in all."
        );
    }
    out
}

/// The buses the auto-binder found, ready to be passed back as `--map`.
/// How many of each list a report prints before it stops naming them.
const SHOWN: usize = 20;

/// Two recordings, and where they part.
///
/// The first line is the answer, because that is the whole question: they
/// agree, or here is the first moment they do not. What each side has that the
/// other does not comes before the differences, since a signal that is only in
/// one recording has no difference to report and reading one into it is how an
/// afternoon goes missing.
pub fn compare_text(report: &Comparison, a: &str, b: &str) -> String {
    let mut out = String::new();

    match report.first() {
        // Nothing in common is not agreement, and saying so as if it were is
        // the one answer a reader must not get: it usually means the two were
        // dumped under different scopes rather than that the design behaved.
        None if report.shared == 0 => {
            let _ = writeln!(out, "these two recordings have no signal in common");
        }
        None => {
            let _ = writeln!(out, "{} shared signal(s), and none of them differ", report.shared);
        }
        Some(first) => {
            let _ = writeln!(
                out,
                "{} of {} shared signal(s) differ; the first at tick {}",
                report.differing.len(),
                report.shared,
                first.at
            );
        }
    }

    for (label, only) in [(a, &report.only_in_a), (b, &report.only_in_b)] {
        if only.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\nonly in {label} ({})", only.len());
        for path in only.iter().take(SHOWN) {
            let _ = writeln!(out, "  {path}");
        }
        if only.len() > SHOWN {
            let _ = writeln!(out, "  ... and {} more", only.len() - SHOWN);
        }
    }

    if !report.differing.is_empty() {
        let _ = writeln!(out, "\ndiffering ({})", report.differing.len());
        let width =
            report.differing.iter().take(SHOWN).map(|one| one.path.len()).max().unwrap_or(0);
        for one in report.differing.iter().take(SHOWN) {
            let _ =
                writeln!(out, "  {:width$}  at {:>10}  {} vs {}", one.path, one.at, one.a, one.b);
        }
        if report.differing.len() > SHOWN {
            let _ = writeln!(out, "  ... and {} more", report.differing.len() - SHOWN);
        }
    }

    if !report.problems.is_empty() {
        let _ = writeln!(out, "\nnot accounted for");
        for problem in &report.problems {
            for line in wrap(problem, 76) {
                let _ = writeln!(out, "  {line}");
            }
        }
    }
    out
}

pub fn suggestions_text(suggestions: &[Suggestion], prefix: &str) -> String {
    let mut out = String::new();
    if suggestions.is_empty() {
        out.push_str("no bus was recognised on this module\n\n");
        out.push_str(
            "  Buses are found by the shape of their names: `*_tvalid` with `*_tready`,\n  \
             `*_byte_valid` with `*_byte_data`, `*_valid` with `*_pixel` or `*_data`, or a\n  \
             pair called scl and sda. Bind the channels by hand with --map if this design\n  \
             names them another way.\n",
        );
        return out;
    }

    let _ = writeln!(out, "{} bus(es) recognised\n", suggestions.len());
    for suggestion in suggestions {
        let _ = writeln!(
            out,
            "  {} `{}`{}",
            suggestion.protocol,
            suggestion.group,
            if suggestion.is_complete() { "" } else { "  (incomplete)" }
        );
        for binding in suggestion.mapped(prefix) {
            let _ = writeln!(
                out,
                "    --map {}={}{}",
                binding.role,
                if binding.invert { "!" } else { "" },
                binding.path
            );
        }
        if !suggestion.missing.is_empty() {
            let _ = writeln!(out, "    missing: {}", suggestion.missing.join(", "));
        }
        out.push('\n');
    }
    out
}

/// What a decoder found.
pub fn decode_text(report: &DecodeReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}\n", report.protocol);

    for (name, value) in &report.stats {
        let _ = writeln!(out, "  {value:>12}  {name}");
    }

    if !report.transactions.is_empty() {
        let _ = writeln!(out, "\n{} transaction(s)", report.transactions.len());
        for transaction in report.transactions.iter().take(40) {
            let fields: Vec<String> =
                transaction.fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
            let _ = writeln!(
                out,
                "  {:>10}  {:<12} {}",
                transaction.t_start,
                transaction.kind,
                fields.join("  ")
            );
        }
        if report.transactions.len() > 40 {
            let _ = writeln!(
                out,
                "  ... and {} more; pass --json for all of them",
                report.transactions.len() - 40
            );
        }
    }

    let errors: Vec<&rtlscope_wave::decode::Annotation> =
        report.annotations.iter().filter(|a| a.level == Level::Error).collect();
    if !errors.is_empty() {
        let _ = writeln!(out, "\n{} thing(s) went wrong", errors.len());
        for annotation in errors.iter().take(20) {
            let _ = writeln!(out, "  {:>10}  {}", annotation.t_start, annotation.label);
            for (name, value) in &annotation.fields {
                let _ = writeln!(out, "                {name}: {value}");
            }
        }
        if errors.len() > 20 {
            let _ = writeln!(out, "  ... and {} more", errors.len() - 20);
        }
    }

    if report.problems.is_empty() {
        out.push_str("\nNothing was skipped.\n");
    } else {
        let _ = writeln!(out, "\n{} thing(s) could not be accounted for", report.problems.len());
        for problem in &report.problems {
            let _ = writeln!(out, "  {problem}");
        }
    }

    if !report.bindings.is_empty() {
        out.push_str("\nread from\n");
        for (role, path) in &report.bindings {
            let _ = writeln!(out, "  {role:<8} {path}");
        }
    }
    out
}

/// The pipeline's stages against the dump's cycles.
///
/// A grid, because that is what a pipeline diagram is: one row per stage, one
/// column per cycle. The legend and the per-row footing carry what a picture
/// cannot say — which signal decided each row, and what a mark there means.
pub fn stages_text(view: &StageView) -> String {
    let mut out = String::new();
    let last = view.first + view.len().saturating_sub(1);
    let _ = writeln!(
        out,
        "{} — {} stage(s) deep over {} cycle(s); showing {} .. {}",
        view.clock, view.depth, view.cycles, view.first, last
    );
    let _ = writeln!(out, "read at `{}`", view.clock_path);

    if view.is_empty() || view.rows.is_empty() {
        out.push_str("\nnothing to draw: the window is empty\n");
        return out;
    }

    // A ruler, so a column can be named. Every tenth cycle, written from the
    // column it belongs to.
    let width = view.len();
    let mut ruler = vec![' '; width];
    for column in 0..width {
        let cycle = view.first + column;
        if !cycle.is_multiple_of(10) {
            continue;
        }
        for (offset, character) in cycle.to_string().chars().enumerate() {
            if let Some(slot) = ruler.get_mut(column + offset) {
                *slot = character;
            }
        }
    }
    let _ = writeln!(out, "\n{:LABEL$}{}", "", ruler.iter().collect::<String>(), LABEL = LABEL);

    for row in &view.rows {
        let strip: String = row.cells.iter().map(|cell| cell.glyph()).collect();
        let _ = writeln!(out, "{:LABEL$}{strip}", format!("  stage {:<3}", row.stage));
    }

    out.push_str(
        "\n  #  carrying    ~  carrying what it carried last cycle — a stall\n  \
         .  empty       ?  undriven     (blank)  not in the dump\n",
    );

    out.push('\n');
    for row in &view.rows {
        let basis = match &row.basis {
            Basis::Valid { signal } => format!("valid `{signal}`"),
            Basis::Activity { watching } => {
                format!("movement of {} signal(s)", watching.len())
            }
            Basis::Absent => "not in the dump".to_string(),
        };
        let value = match &row.payload {
            Some(name) => format!("value `{name}`"),
            None => String::new(),
        };
        let _ = writeln!(
            out,
            "  stage {:<3} {:<28} {:<26} {} busy, {} held   ({} register(s), {} in the dump)",
            row.stage, basis, value, row.occupied, row.held, row.registers, row.matched
        );
    }

    // A row showing movement is not a row showing occupancy, and the difference
    // is worth more than a footnote: it is fixable, and this says how.
    let guessing: Vec<usize> = view
        .rows
        .iter()
        .filter(|row| matches!(row.basis, Basis::Activity { .. }))
        .map(|row| row.stage)
        .collect();
    if !guessing.is_empty() {
        let listed: Vec<String> = guessing.iter().map(usize::to_string).collect();
        let _ = writeln!(
            out,
            "\n  Stage(s) {} have nothing named like a valid, so they show which registers \
             moved\n  rather than whether the stage held anything — which is not the same \
             claim. Name\n  the bit yourself with --valid {}=<signal> if the design calls it \
             something else.",
            listed.join(", "),
            guessing[0]
        );
    }

    if view.problems.is_empty() {
        out.push_str("\nNothing was left out.\n");
    } else {
        let _ = writeln!(out, "\n{} thing(s) could not be accounted for", view.problems.len());
        for problem in &view.problems {
            let _ = writeln!(out, "  {problem}");
        }
    }
    out
}

/// How wide the row labels are, so the ruler lines up with the grid.
const LABEL: usize = 12;

/// The protocols a decoder exists for.
pub fn protocols_text() -> String {
    let mut out = String::from("protocols\n\n");
    for decoder in rtlscope_wave::decode::all() {
        let _ = writeln!(out, "  {}", decoder.protocol());
        for line in wrap(decoder.doc(), 72) {
            let _ = writeln!(out, "    {line}");
        }
        out.push_str("    channels: ");
        let channels: Vec<String> = decoder
            .channels()
            .iter()
            .map(|c| if c.required { c.role.to_string() } else { format!("[{}]", c.role) })
            .collect();
        let _ = writeln!(out, "{}", channels.join(" "));
        out.push('\n');
    }
    out.push_str("  Names in brackets are optional; leaving one out narrows what can be\n");
    out.push_str("  said, and the report says which.\n");
    out
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// What a recording said the distance was, and what that means next to the
/// structure.
pub fn latency_text(report: &rtlscope_wave::LatencyReport, verdict: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "
measured over {}
",
        report.clock
    ));

    let Some(min) = report.min else {
        out.push_str(
            "  no beats were paired, so there is nothing to measure
",
        );
        for problem in &report.problems {
            out.push_str(&format!(
                "  note: {problem}
"
            ));
        }
        return out;
    };
    let (median, max) = (report.median.unwrap_or(min), report.max.unwrap_or(min));
    out.push_str(&format!(
        "  {} sample(s): {min} shortest, {median} typical, {max} longest
",
        report.samples
    ));

    // The bars are what makes a spread visible; the numbers alone read as one
    // answer with error bars, which is the reading a histogram exists to
    // prevent.
    let widest = report.histogram.iter().map(|bin| bin.count).max().unwrap_or(1).max(1);
    for bin in &report.histogram {
        let width = (bin.count * 32).div_ceil(widest);
        out.push_str(&format!(
            "  {:>4} cycle(s) {:<32} {}
",
            bin.cycles,
            "#".repeat(width),
            bin.count
        ));
    }

    for problem in &report.problems {
        out.push_str(&format!(
            "  note: {problem}
"
        ));
    }
    for line in verdict {
        out.push_str(&format!(
            "  {line}
"
        ));
    }
    out
}
