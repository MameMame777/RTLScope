//! Turns an `sv-parser` [`Locate`] into an RTLScope [`Span`] — and, when the
//! SystemVerilog was written by a tool from something else, into a span in
//! that something else.
//!
//! `Locate::line` cannot be used for this. It counts lines in the *preprocessed*
//! text, so every line number past an `include` is shifted by however many lines
//! that include pulled in. Measured on `tests/data/with_include.sv`, whose
//! `module` keyword sits on line 4: `Locate::line` reports 7.
//!
//! [`SyntaxTree::get_origin`] is the reliable mapping. It returns the original
//! file and a byte offset within it, which this module converts to a line and
//! column against a table of line starts built from the file on disk. That is
//! also why spans survive macro expansion: the offset points at the text the
//! author actually wrote.
//!
//! # Generated SystemVerilog
//!
//! A transpiler that leaves a Source Map beside its output — Veryl does, as
//! `foo.sv.map`, and names it on the last line of `foo.sv` — has already said
//! where every token came from. A reader of the design wants that answer: the
//! file they wrote, not the one the tool wrote. So a location inside a
//! generated file is looked up in its map, and the span that comes back points
//! into the original. Nothing downstream knows the difference; the file table
//! simply holds a `.veryl`, and every view that follows a span lands there.
//!
//! The map is trusted line by line. Where it is silent — a line the generator
//! made up, with no counterpart — the span stays in the generated file, which
//! is at least a real place, rather than being pinned to whatever the map said
//! about the line before.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rtlscope_ir::{FileId, FileTable, Generated, Span};
use sv_parser::{Locate, SyntaxTree};

#[derive(Debug, Default)]
pub struct SourceMap {
    files: FileTable,
    /// Parallel to the ids handed out by `files`.
    sources: Vec<Source>,
    /// The files reached through a map: the ones an author wrote, whose names
    /// are the ones a reader knows. See [`SourceMap::written_at`].
    originals: HashSet<FileId>,
    /// Each generated file and the original it was reached from, once.
    generated: Vec<Generated>,
}

#[derive(Debug, Default)]
struct Source {
    text: String,
    /// Byte offset of the start of each line.
    line_starts: Vec<usize>,
    /// False when the file could not be read, so spans into it are unusable.
    readable: bool,
    /// Where this file's tokens came from, when a tool wrote the file.
    origin: Option<Origin>,
}

/// A Source Map (revision 3) beside a generated file, and where its `sources`
/// are measured from.
struct Origin {
    map: sourcemap::SourceMap,
    /// The directory the map's `sources` entries are relative to: the map's
    /// own, which is what the specification says and what Veryl writes.
    base: PathBuf,
    /// The map file itself, for whoever wants to look the other way later.
    path: PathBuf,
}

impl std::fmt::Debug for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Origin").field("base", &self.base).finish_non_exhaustive()
    }
}

impl Source {
    fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut line_starts = vec![0usize];
                line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
                let origin = Origin::beside(path, &text);
                Source { text, line_starts, readable: true, origin }
            }
            Err(_) => Source::default(),
        }
    }

    /// 1-based line and column for a byte offset. The column counts characters,
    /// not bytes, so a line with a multi-byte comment before the token still
    /// points where an editor would put the caret.
    fn line_col(&self, offset: usize) -> (u32, u32) {
        let line_index = match self.line_starts.binary_search(&offset) {
            Ok(exact) => exact,
            Err(next) => next.saturating_sub(1),
        };
        let line_start = self.line_starts[line_index];
        let column = self
            .text
            .get(line_start..offset.min(self.text.len()))
            .map_or(0, |prefix| prefix.chars().count());
        (line_index as u32 + 1, column as u32 + 1)
    }

    /// How long the identifier starting at a 1-based line and column is, in
    /// bytes — or `None` when what starts there is not an identifier.
    ///
    /// A span carries the length of the token the *generated* file has, and a
    /// generator renames things: Veryl writes `lights_Control` for a module the
    /// author called `Control`. Marking fourteen characters of the original
    /// from where `Control` starts would run the highlight into whatever
    /// follows, so the length is measured again where the span now points.
    fn identifier_len(&self, line: u32, col: u32) -> Option<u32> {
        self.identifier_at(line, col).map(|name| name.len() as u32)
    }

    /// The identifier starting at a 1-based line and column, if one does.
    fn identifier_at(&self, line: u32, col: u32) -> Option<&str> {
        let rest = self.from(line, col)?;
        identifier_prefix(rest)
    }

    /// The identifier after the one at a 1-based line and column — the name
    /// after a `module` keyword, which is where a module's span starts.
    fn identifier_after(&self, line: u32, col: u32) -> Option<&str> {
        let rest = self.from(line, col)?;
        let keyword = identifier_prefix(rest)?;
        identifier_prefix(rest[keyword.len()..].trim_start())
    }

    /// The text of a line from a 1-based column to its end.
    fn from(&self, line: u32, col: u32) -> Option<&str> {
        let start = *self.line_starts.get(line.checked_sub(1)? as usize)?;
        let end = self.line_starts.get(line as usize).map_or(self.text.len(), |next| *next);
        let text = self.text.get(start..end)?;
        let byte = text.char_indices().nth(col.checked_sub(1)? as usize)?.0;
        Some(&text[byte..])
    }
}

/// Where a line of an original file landed in the file generated from it.
///
/// The first place on that line the map mentions, in 1-based line and column
/// of the generated file — enough to put the generated file beside the
/// original at the same point. The map is read from disk each time: this is
/// asked when a reader presses a button, not on every frame.
pub fn generated_position(map: &Path, original_line: u32) -> Option<(u32, u32)> {
    let bytes = std::fs::read(map).ok()?;
    let map = sourcemap::SourceMap::from_slice(&bytes).ok()?;
    let wanted = original_line.checked_sub(1)?;
    map.tokens()
        .filter(|token| token.get_src_line() == wanted)
        .map(|token| (token.get_dst_line() + 1, token.get_dst_col() + 1))
        .min()
}

/// Where a place in a generated file came from: the original file, and the
/// 1-based line and column in it. The other direction of
/// [`generated_position`].
pub fn original_position(map: &Path, line: u32, col: u32) -> Option<(PathBuf, u32, u32)> {
    let bytes = std::fs::read(map).ok()?;
    let parsed = sourcemap::SourceMap::from_slice(&bytes).ok()?;
    let origin = Origin { map: parsed, base: map.parent()?.to_path_buf(), path: map.to_path_buf() };
    origin.lookup(line, col)
}

/// The identifier a piece of text starts with, if it starts with one.
fn identifier_prefix(text: &str) -> Option<&str> {
    let first = text.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let len = text.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
    Some(&text[..len])
}

impl Origin {
    /// The map a generated file names on its last line, or the one beside it.
    ///
    /// The last line first, because that is the file saying which map is its
    /// own; `foo.sv.map` next to `foo.sv` second, for a generator that writes
    /// the map and not the note. A map that will not parse is treated as no
    /// map: the generated file is still readable on its own, and a reader who
    /// gets spans into it has lost less than one who gets no design.
    fn beside(path: &Path, text: &str) -> Option<Origin> {
        let dir = path.parent()?;
        let named = text
            .lines()
            .rev()
            .take(3)
            .find_map(|line| line.trim().strip_prefix("//# sourceMappingURL="))
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let map_path = match named {
            Some(relative) => dir.join(relative),
            None => {
                let mut name = path.file_name()?.to_os_string();
                name.push(".map");
                dir.join(name)
            }
        };
        let bytes = std::fs::read(&map_path).ok()?;
        let map = sourcemap::SourceMap::from_slice(&bytes).ok()?;
        let base = map_path.parent()?.to_path_buf();
        Some(Origin { map, base, path: map_path })
    }

    /// Where a 1-based line and column of the generated file came from: the
    /// original file, and the 1-based line and column in it.
    ///
    /// `None` when the map has nothing to say about that line. The lookup
    /// finds the nearest token at or before the place asked about, and for a
    /// line the map skips that is a token from an earlier line — an answer,
    /// but to a different question.
    fn lookup(&self, line: u32, col: u32) -> Option<(PathBuf, u32, u32)> {
        let token = self.map.lookup_token(line.checked_sub(1)?, col.checked_sub(1)?)?;
        if token.get_dst_line() + 1 != line {
            return None;
        }
        let (src_line, src_col) = token.get_src();
        if src_line == u32::MAX || src_col == u32::MAX {
            return None;
        }
        let source = token.get_source()?;
        Some((self.base.join(source), src_line + 1, src_col + 1))
    }
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// The interned paths, to be handed to the finished design so spans can be
    /// rendered.
    pub fn files(&self) -> &FileTable {
        &self.files
    }

    pub fn into_files(self) -> FileTable {
        self.files
    }

    /// The table, and which of its files were generated from which others.
    pub fn into_parts(self) -> (FileTable, Vec<Generated>) {
        (self.files, self.generated)
    }

    /// Resolves a token's location to a span in the file the author wrote.
    ///
    /// Returns [`Span::UNKNOWN`] when the token has no origin (synthesised text)
    /// or its file cannot be read; `rtlscope-elab::validate` rejects a design that
    /// still contains one, so this never passes silently.
    pub fn resolve(&mut self, tree: &SyntaxTree, locate: &Locate) -> Span {
        let Some((origin_path, origin_offset)) = tree.get_origin(locate) else {
            return Span::UNKNOWN;
        };
        self.resolve_offset(origin_path, origin_offset, locate.len as u32)
    }

    pub fn resolve_offset(&mut self, path: &Path, offset: usize, len: u32) -> Span {
        let file = self.intern(path);
        let source = &self.sources[file.0 as usize];
        if !source.readable {
            return Span::UNKNOWN;
        }
        let (line, col) = source.line_col(offset);

        // Through the map, when the file has one and the map speaks for this
        // line. The original has to be readable too: a map pointing at a file
        // that is not there is a map to nowhere, and the generated file is the
        // better of the two places left.
        if let Some((original, at_line, at_col)) =
            source.origin.as_ref().and_then(|origin| origin.lookup(line, col))
        {
            let there = self.intern(&original);
            let source = &self.sources[there.0 as usize];
            if source.readable {
                let len = source.identifier_len(at_line, at_col).unwrap_or(len);
                if self.originals.insert(there)
                    || !self.generated.iter().any(|g| g.generated == file)
                {
                    let map = self.sources[file.0 as usize]
                        .origin
                        .as_ref()
                        .map(|origin| origin.path.clone())
                        .unwrap_or_default();
                    if !self.generated.iter().any(|g| g.generated == file && g.original == there) {
                        self.generated.push(Generated { generated: file, original: there, map });
                    }
                }
                return Span::new(there, at_line, at_col, len);
            }
        }
        Span::new(file, line, col, len)
    }

    /// The name written at a span, when the span is in a file the author
    /// wrote rather than one a tool did.
    ///
    /// A generator renames things — Veryl puts the project in front of every
    /// module and rewrites a reset by its convention — and the span already
    /// says where the author's own name is. `None` for a span in an ordinary
    /// file: there is no second name to know.
    pub fn written_at(&self, span: Span) -> Option<String> {
        if !self.originals.contains(&span.file) {
            return None;
        }
        let source = self.sources.get(span.file.0 as usize)?;
        source.identifier_at(span.line, span.col).map(str::to_string)
    }

    /// The name written after the keyword a span starts at: a module's span is
    /// the `module` keyword, and its name is the next word.
    pub fn written_after_keyword(&self, span: Span) -> Option<String> {
        if !self.originals.contains(&span.file) {
            return None;
        }
        let source = self.sources.get(span.file.0 as usize)?;
        source.identifier_after(span.line, span.col).map(str::to_string)
    }

    fn intern(&mut self, path: &Path) -> FileId {
        // Canonicalise through dunce so the same file reached by two spellings
        // gets one id, without Windows' \\?\ prefix leaking into output.
        let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let id = self.files.intern(canonical.clone());
        if id.0 as usize == self.sources.len() {
            self.sources.push(Source::load(&canonical));
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_is_one_based() {
        let source = Source {
            text: "abc\ndef\n".to_string(),
            line_starts: vec![0, 4, 8],
            readable: true,
            origin: None,
        };
        assert_eq!(source.identifier_after(1, 1), None, "nothing after `abc` on its line");
        assert_eq!(source.line_col(0), (1, 1));
        assert_eq!(source.line_col(2), (1, 3));
        assert_eq!(source.line_col(4), (2, 1));
        assert_eq!(source.line_col(6), (2, 3));
    }

    #[test]
    fn columns_count_characters_not_bytes() {
        // A multi-byte comment ahead of the token on the same line.
        let text = "// 日本語\nlogic x;\n".to_string();
        let mut line_starts = vec![0usize];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        let offset = text.find('x').unwrap();
        let source = Source { text, line_starts, readable: true, origin: None };
        assert_eq!(source.line_col(offset), (2, 7));
    }

    /// The length a span carries is the generated token's; the original's is
    /// measured where the span lands, and only for a name.
    #[test]
    fn an_identifier_is_measured_where_the_map_points() {
        let text = "module Control (\n    i_start: input logic,\n    x = 3'b1;\n".to_string();
        let mut line_starts = vec![0usize];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        let source = Source { text, line_starts, readable: true, origin: None };
        assert_eq!(source.identifier_len(1, 8), Some(7), "Control");
        assert_eq!(source.identifier_at(1, 8), Some("Control"));
        assert_eq!(source.identifier_after(1, 1), Some("Control"), "the word after `module`");
        assert_eq!(source.identifier_len(2, 5), Some(7), "i_start");
        assert_eq!(source.identifier_len(3, 9), None, "a number is not a name");
        assert_eq!(source.identifier_len(9, 1), None, "past the end");
        assert_eq!(source.identifier_len(0, 1), None, "lines are one-based");
    }
}
