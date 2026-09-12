//! Reading the RTL itself, in the window.
//!
//! Every other view here is *derived* — a diagram, a state machine, a waveform
//! — and each one names the line it came from. Until now those names were a
//! path and a number that took the reader out of the application to resolve.
//! This closes that: the source is the last view, and the one every other view
//! points at.
//!
//! Nothing keeps the text after the front end has run. [`FileTable`] holds
//! paths and the parser's own source map is dropped once lowering is done, so
//! this reads the file from disk and keeps what it read. That is also the
//! honest arrangement: the file on disk may have been edited since the design
//! was elaborated, and reading it now shows what is *there* rather than what
//! was parsed.
//!
//! The highlighter is deliberately small — keywords, comments, strings,
//! numbers — and is a pure function so the two cases that always break these
//! can be pinned by tests: a block comment spanning lines, and a `//` that is
//! inside a string and starts nothing.

use std::collections::HashMap;
use std::path::Path;

use rtlscope_ir::{FileId, FileTable};

/// What a run of characters on a line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    Plain,
    Keyword,
    Comment,
    Text,
    Number,
}

/// The keywords worth colouring: the ones that give a file its shape.
///
/// Not the whole of IEEE 1800. A highlighter that knows every keyword and a
/// highlighter that knows the structural ones look the same from two feet away,
/// and the short list cannot rot.
const KEYWORDS: &[&str] = &[
    "module",
    "endmodule",
    "input",
    "output",
    "inout",
    "wire",
    "logic",
    "reg",
    "parameter",
    "localparam",
    "assign",
    "always",
    "always_ff",
    "always_comb",
    "always_latch",
    "initial",
    "begin",
    "end",
    "if",
    "else",
    "case",
    "casez",
    "endcase",
    "default",
    "for",
    "while",
    "generate",
    "endgenerate",
    "genvar",
    "function",
    "endfunction",
    "task",
    "endtask",
    "return",
    "posedge",
    "negedge",
    "signed",
    "unsigned",
    "typedef",
    "enum",
    "struct",
    "packed",
    "int",
    "integer",
    "bit",
    "byte",
    "automatic",
    "static",
    "const",
    "package",
    "endpackage",
    "import",
    "interface",
    "endinterface",
    "modport",
    "defparam",
    "unique",
    "priority",
    "break",
    "continue",
    "repeat",
    "forever",
];

/// The same list for Veryl, which the viewer shows whenever a design was
/// written in it: the source map puts every span in the `.veryl`, and a
/// `.veryl` coloured by SystemVerilog's keywords would light `endmodule` and
/// leave `inst` dark. Comments, strings and numbers are spelled the same in
/// both, so the keywords are the only thing that differs.
const VERYL_KEYWORDS: &[&str] = &[
    "module",
    "interface",
    "package",
    "modport",
    "import",
    "export",
    "inst",
    "param",
    "const",
    "var",
    "let",
    "assign",
    "always_ff",
    "always_comb",
    "if_reset",
    "if",
    "else",
    "case",
    "switch",
    "default",
    "for",
    "in",
    "rev",
    "step",
    "break",
    "return",
    "function",
    "enum",
    "struct",
    "union",
    "type",
    "input",
    "output",
    "inout",
    "clock",
    "reset",
    "logic",
    "bit",
    "signed",
    "unsigned",
    "initial",
    "final",
    "embed",
    "include",
    "pub",
    "proto",
    "bind",
    "as",
    "inside",
    "outside",
    "unsafe",
];

/// Which language a file is coloured as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    #[default]
    SystemVerilog,
    Veryl,
}

impl Language {
    /// By the file's name, which is the only thing that says.
    pub fn of(path: Option<&Path>) -> Language {
        match path.and_then(|p| p.extension()) {
            Some(ext) if ext.eq_ignore_ascii_case("veryl") => Language::Veryl,
            _ => Language::SystemVerilog,
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Language::SystemVerilog => KEYWORDS,
            Language::Veryl => VERYL_KEYWORDS,
        }
    }
}

/// Splits one line into runs, carrying block-comment state across lines.
///
/// The state is in and out because `/* */` does not respect line boundaries and
/// a per-line highlighter that forgot that would colour half a comment as code.
pub fn pieces(
    line: &str,
    in_block: &mut bool,
    lang: Language,
) -> Vec<(std::ops::Range<usize>, Piece)> {
    let bytes = line.as_bytes();
    let mut out: Vec<(std::ops::Range<usize>, Piece)> = Vec::new();
    let mut at = 0usize;
    let mut plain = 0usize;

    // Whatever has been passed over since the last run was emitted.
    macro_rules! flush {
        ($to:expr) => {
            if plain < $to {
                out.push((plain..$to, Piece::Plain));
            }
        };
    }

    while at < bytes.len() {
        if *in_block {
            match line[at..].find("*/") {
                Some(offset) => {
                    let end = at + offset + 2;
                    out.push((at..end, Piece::Comment));
                    *in_block = false;
                    at = end;
                    plain = at;
                }
                None => {
                    out.push((at..bytes.len(), Piece::Comment));
                    return out;
                }
            }
            continue;
        }

        if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'/') {
            flush!(at);
            out.push((at..bytes.len(), Piece::Comment));
            return out;
        }
        if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'*') {
            flush!(at);
            *in_block = true;
            continue;
        }
        if bytes[at] == b'"' {
            flush!(at);
            let mut end = at + 1;
            while end < bytes.len() {
                // A quote after a backslash is part of the string, not its end.
                if bytes[end] == b'\\' {
                    end += 2;
                    continue;
                }
                if bytes[end] == b'"' {
                    end += 1;
                    break;
                }
                end += 1;
            }
            let end = end.min(bytes.len());
            out.push((at..end, Piece::Text));
            at = end;
            plain = at;
            continue;
        }
        // A word: a keyword, or a sized literal like `8'h3f`, or neither.
        if bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_' || bytes[at] == b'\'' {
            let start = at;
            while at < bytes.len()
                && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_' || bytes[at] == b'\'')
            {
                at += 1;
            }
            let word = &line[start..at];
            let kind = if lang.keywords().contains(&word) {
                Piece::Keyword
            } else if word.starts_with(|c: char| c.is_ascii_digit()) || word.starts_with('\'') {
                Piece::Number
            } else {
                continue;
            };
            flush!(start);
            out.push((start..at, kind));
            plain = at;
            continue;
        }
        at += 1;
    }
    flush!(bytes.len());
    out
}

/// The files read so far, and what came of reading them.
///
/// One per design rather than one per window: two views pointing into the same
/// file should not read it twice, and the text is what makes a `file:line`
/// worth clicking.
#[derive(Default)]
pub struct Sources {
    files: HashMap<FileId, Result<Vec<String>, String>>,
}

impl Sources {
    /// The lines of a file, read on first sight.
    ///
    /// The error is kept as text and handed back rather than retried: a file
    /// that is not there will still not be there next frame, and a viewer that
    /// tried sixty times a second would be a viewer that hammers the disk.
    pub fn lines(&mut self, files: &FileTable, file: FileId) -> Result<&[String], &str> {
        let read = self.files.entry(file).or_insert_with(|| match files.path(file) {
            Some(path) => read_lines(path),
            None => Err("this location has no file behind it".to_string()),
        });
        match read {
            Ok(lines) => Ok(lines.as_slice()),
            Err(why) => Err(why.as_str()),
        }
    }
}

fn read_lines(path: &Path) -> Result<Vec<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text.lines().map(str::to_string).collect()),
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

/// The identifier a column of a line falls inside.
///
/// Columns are counted in characters rather than bytes, because that is what a
/// monospace viewer can work out from a click: every glyph is one cell wide, so
/// the cell under the pointer is the character under the pointer. The range
/// that comes back is in bytes, for slicing.
///
/// SystemVerilog identifiers are `[A-Za-z_][A-Za-z0-9_$]*`. A column landing on
/// a digit that begins a run — `8'h00`, `2'd1` — is not an identifier and comes
/// back as nothing, rather than as the tail of a number.
pub fn word_at(line: &str, column: usize) -> Option<(std::ops::Range<usize>, &str)> {
    let mut start: Option<usize> = None;
    let mut found: Option<std::ops::Range<usize>> = None;

    for (index, (offset, character)) in line.char_indices().enumerate() {
        let part = character.is_ascii_alphanumeric() || character == '_' || character == '$';
        match (part, start) {
            (true, None) => start = Some(offset),
            (false, Some(from)) => {
                if let Some(range) = ends(from, offset, index, column) {
                    found = Some(range);
                    break;
                }
                start = None;
            }
            _ => {}
        }
    }
    if found.is_none()
        && let Some(from) = start
    {
        found = ends(from, line.len(), line.chars().count(), column);
    }

    let range = found?;
    let word = &line[range.clone()];
    // A run beginning with a digit is a number. A run just after a tick is the
    // base and digits of a sized literal — `h00` of `8'h00` — which begins with
    // a letter and is no more a name than the `8` in front of it.
    let after_tick = range.start > 0 && line.as_bytes()[range.start - 1] == b'\'';
    match word.starts_with(|c: char| c.is_ascii_digit()) || after_tick {
        true => None,
        false => Some((range, word)),
    }
}

/// The bytes of `line` that a span covers.
///
/// `col` is 1-based and counts characters, as the front end reports it, and
/// `len` counts characters too. The result is clipped to the line. A span with
/// no length falls back to the word at that column, so a target that names a
/// place rather than a run still marks something the reader can see.
pub fn span_range(line: &str, col: u32, len: u32) -> Option<std::ops::Range<usize>> {
    let column = (col as usize).checked_sub(1)?;
    if len == 0 {
        return word_at(line, column).map(|(range, _)| range);
    }
    // Every character boundary, and the end of the line as one more, so a span
    // that runs off the end stops there rather than nowhere.
    let mut boundaries =
        line.char_indices().map(|(offset, _)| offset).chain(std::iter::once(line.len()));
    let start = boundaries.by_ref().nth(column)?;
    let end = boundaries.nth(len as usize - 1).unwrap_or(line.len());
    (start < end).then_some(start..end)
}

/// Every whole-word occurrence of `name` in `line`, as byte ranges.
///
/// Whole words, so `state` does not light up inside `next_state`: the point is
/// to show where *this* signal is read and written, and a name that merely
/// contains it is a different signal.
pub fn occurrences(line: &str, name: &str) -> Vec<std::ops::Range<usize>> {
    let is_part = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    let mut found = Vec::new();
    if name.is_empty() {
        return found;
    }
    let mut from = 0;
    while let Some(at) = line[from..].find(name) {
        let start = from + at;
        let end = start + name.len();
        let clear_before = line[..start].chars().next_back().is_none_or(|c| !is_part(c));
        let clear_after = line[end..].chars().next().is_none_or(|c| !is_part(c));
        if clear_before && clear_after {
            found.push(start..end);
        }
        from = end;
    }
    found
}

/// The run `from..to` if `column` is one of the characters it covers.
fn ends(from: usize, to: usize, past: usize, column: usize) -> Option<std::ops::Range<usize>> {
    // `past` is the column one beyond the run, so the run began that many
    // characters back minus its length. Counting forwards from the start of the
    // line would mean a second pass; this is the same number.
    let length = to - from;
    let began = past.checked_sub(length)?;
    (began <= column && column < past).then_some(from..to)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `.veryl` is coloured by its own words: `inst` is a keyword there and
    /// nowhere in SystemVerilog, and `endmodule` is not one.
    #[test]
    fn a_veryl_file_is_coloured_by_veryls_own_keywords() {
        let line = "    inst u_timer: Timer; // endmodule is not a word here";
        let mut state = false;
        let keywords: Vec<String> = pieces(line, &mut state, Language::Veryl)
            .into_iter()
            .filter(|(_, kind)| *kind == Piece::Keyword)
            .map(|(range, _)| line[range].to_string())
            .collect();
        assert_eq!(keywords, ["inst"]);

        let mut state = false;
        let sv: Vec<String> = pieces("inst endmodule", &mut state, Language::SystemVerilog)
            .into_iter()
            .filter(|(_, kind)| *kind == Piece::Keyword)
            .map(|(range, _)| "inst endmodule"[range].to_string())
            .collect();
        assert_eq!(sv, ["endmodule"], "and the other way round");

        assert_eq!(Language::of(Some(Path::new("src/top.veryl"))), Language::Veryl);
        assert_eq!(Language::of(Some(Path::new("SRC/TOP.VERYL"))), Language::Veryl);
        assert_eq!(Language::of(Some(Path::new("top.sv"))), Language::SystemVerilog);
        assert_eq!(Language::of(None), Language::SystemVerilog);
    }

    fn kinds(line: &str, in_block: &mut bool) -> Vec<(String, Piece)> {
        pieces(line, in_block, Language::SystemVerilog)
            .into_iter()
            .map(|(range, kind)| (line[range].to_string(), kind))
            .collect()
    }

    fn only(line: &str, want: Piece) -> Vec<String> {
        let mut state = false;
        kinds(line, &mut state)
            .into_iter()
            .filter(|(_, kind)| *kind == want)
            .map(|(text, _)| text)
            .collect()
    }

    #[test]
    fn the_words_that_give_a_file_its_shape_are_picked_out() {
        assert_eq!(
            only("    always_ff @(posedge clk) begin", Piece::Keyword),
            ["always_ff", "posedge", "begin"]
        );
        // A name that merely contains a keyword is not one.
        assert!(only("assign endpoint_ready = 1;", Piece::Keyword).contains(&"assign".to_string()));
        assert!(!only("assign endpoint_ready = 1;", Piece::Keyword).contains(&"end".to_string()));
    }

    #[test]
    fn a_sized_literal_is_one_run_not_three() {
        assert_eq!(only("data <= 8'h3f;", Piece::Number), ["8'h3f"]);
        assert_eq!(only("x = 42;", Piece::Number), ["42"]);
    }

    /// The case that breaks every naive highlighter: a comment that outlives
    /// its line.
    #[test]
    fn a_block_comment_carries_across_lines() {
        let mut state = false;
        assert_eq!(only_with("code /* opens", &mut state, Piece::Comment), ["/* opens"]);
        assert!(state, "the state has to leave the line still open");

        assert_eq!(only_with("still inside", &mut state, Piece::Comment), ["still inside"]);
        assert!(state);

        let closing = kinds("closes */ assign x = 1;", &mut state);
        assert!(!state, "and close again");
        assert_eq!(closing[0], ("closes */".to_string(), Piece::Comment));
        assert!(closing.iter().any(|(text, kind)| text == "assign" && *kind == Piece::Keyword));
    }

    /// The other one: a `//` that is inside a string and starts nothing.
    #[test]
    fn a_slash_inside_a_string_does_not_start_a_comment() {
        let mut state = false;
        let runs = kinds(r#"$display("a // b"); assign x = 1;"#, &mut state);
        assert!(runs.iter().any(|(text, kind)| text == r#""a // b""# && *kind == Piece::Text));
        assert!(runs.iter().any(|(text, kind)| text == "assign" && *kind == Piece::Keyword));
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        let mut state = false;
        let runs = kinds(r#"$display("say \"hi\" now"); end"#, &mut state);
        assert!(
            runs.iter().any(|(text, kind)| *kind == Piece::Text && text.contains("now")),
            "{runs:?}"
        );
        assert!(runs.iter().any(|(text, kind)| text == "end" && *kind == Piece::Keyword));
    }

    /// Every byte of the line has to be covered exactly once, or drawing it
    /// would drop or double characters.
    #[test]
    fn the_runs_tile_the_line_exactly() {
        let mut state = false;
        for line in [
            "always_ff @(posedge clk) q <= 8'hff;  // a note",
            r#"  if (x == "s") /* mid */ y = 1;"#,
            "",
            "   ",
            "endmodule",
        ] {
            let runs = pieces(line, &mut state, Language::SystemVerilog);
            let mut at = 0;
            for (range, _) in &runs {
                assert_eq!(range.start, at, "gap or overlap in `{line}`: {runs:?}");
                at = range.end;
            }
            assert_eq!(at, line.len(), "did not reach the end of `{line}`: {runs:?}");
        }
    }

    fn only_with(line: &str, state: &mut bool, want: Piece) -> Vec<String> {
        kinds(line, state)
            .into_iter()
            .filter(|(_, kind)| *kind == want)
            .map(|(text, _)| text)
            .collect()
    }

    /// The whole point is a click landing on a name. Every column of the name
    /// has to give the name, and no column outside it may.
    #[test]
    fn a_column_gives_the_identifier_it_lands_in() {
        let line = "        staged <= in_data;";
        for column in 8..14 {
            assert_eq!(
                word_at(line, column).map(|(_, word)| word),
                Some("staged"),
                "column {column} is inside `staged`"
            );
        }
        assert_eq!(word_at(line, 7).map(|(_, w)| w), None, "the space before it is not a name");
        assert_eq!(word_at(line, 15).map(|(_, w)| w), None, "nor is `<`");
        assert_eq!(word_at(line, 18).map(|(_, w)| w), Some("in_data"));
        // Past the end of the line.
        assert_eq!(word_at(line, 400), None);
    }

    /// `8'h00` is a literal. Offering to put `h00` on a waveform would be
    /// offering something that is not there.
    #[test]
    fn a_run_beginning_with_a_digit_is_not_a_name() {
        let line = "            staged <= 8'h00;";
        assert_eq!(word_at(line, 12).map(|(_, w)| w), Some("staged"));
        assert_eq!(word_at(line, 22).map(|(_, w)| w), None, "`8` begins a number");
        // The `h00` after the tick is its own run, and also a number.
        assert_eq!(word_at(line, 25).map(|(_, w)| w), None);
    }

    /// A line can hold a comment in any script, and the columns after it still
    /// have to line up with what the reader clicked.
    #[test]
    fn a_multibyte_comment_does_not_shift_the_columns() {
        let line = "    // \u{6ce2}\u{5f62} here";
        let (range, word) = word_at(line, 10).expect("`here` is at column 10");
        assert_eq!(word, "here");
        assert_eq!(&line[range], "here", "the byte range slices the same word");
    }

    /// A span is a column and a length; the mark is the characters under
    /// them, and no more than the line has.
    #[test]
    fn a_span_marks_exactly_the_characters_it_covers() {
        let line = "    logic [1:0] state, next_state;";
        assert_eq!(span_range(line, 17, 5).map(|r| &line[r]), Some("state"));
        // Off the end of the line, the mark stops at the end.
        assert_eq!(span_range(line, 24, 400).map(|r| &line[r]), Some("next_state;"));
        // No length means the name at that column.
        assert_eq!(span_range(line, 17, 0).map(|r| &line[r]), Some("state"));
        // Columns are 1-based: there is no column 0, and nothing past the line.
        assert_eq!(span_range(line, 0, 5), None);
        assert_eq!(span_range(line, 80, 5), None);
    }

    /// Columns count characters, not bytes, so a comment in another script
    /// before the name does not move the mark off it.
    #[test]
    fn a_span_after_a_multibyte_comment_still_lands_on_its_word() {
        let line = "    // \u{6ce2}\u{5f62} here";
        assert_eq!(span_range(line, 11, 4).map(|r| &line[r]), Some("here"));
    }

    /// The same name elsewhere on the line, and only the same name: `state`
    /// is not inside `next_state`.
    #[test]
    fn occurrences_are_whole_words_only() {
        let line = "        S_IDLE:  if (start) next_state = S_RUN; // state";
        let found: Vec<&str> = occurrences(line, "state").into_iter().map(|r| &line[r]).collect();
        assert_eq!(found, ["state"], "the one in the comment, not the one inside next_state");
        assert_eq!(occurrences("    assign busy = (state != S_IDLE);", "state").len(), 1);
        assert_eq!(occurrences("    assign busy = (state != S_IDLE);", "busy_n").len(), 0);
        assert_eq!(occurrences("state state", "state").len(), 2);
        assert_eq!(occurrences("state", "").len(), 0);
    }
}
