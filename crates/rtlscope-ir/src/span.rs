//! Source locations.
//!
//! The core invariant of the whole project lives here: every `Net`, `Process`
//! and `Instance` carries a real [`Span`]. Feature ⑤ (reverse engineering) and
//! the MCP server both die the moment a node cannot answer "which line?".

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Index into [`FileTable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileId(pub u32);

impl FileId {
    /// Sentinel for a location that has not been filled in yet.
    ///
    /// Construction passes may use it as a placeholder, but
    /// `rtlscope-elab::validate` rejects any design that still contains one, so
    /// it can never reach a consumer.
    pub const UNKNOWN: FileId = FileId(u32::MAX);
}

/// A half-open source range, stored as a 1-based line/column plus a byte length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Span {
    pub file: FileId,
    pub line: u32,
    pub col: u32,
    pub len: u32,
}

impl Span {
    /// Placeholder span. See [`FileId::UNKNOWN`] — validation rejects these.
    pub const UNKNOWN: Span = Span { file: FileId::UNKNOWN, line: 0, col: 0, len: 0 };

    pub const fn new(file: FileId, line: u32, col: u32, len: u32) -> Self {
        Self { file, line, col, len }
    }

    pub const fn is_unknown(self) -> bool {
        self.file.0 == FileId::UNKNOWN.0
    }
}

/// A file a tool wrote, and the file the author wrote it from.
///
/// Recorded when a span is looked up through the tool's source map, so a
/// reader can be shown either: the design is placed in the original, and the
/// generated file is there to be opened beside it. `map` is where the
/// correspondence lives, for turning a line of one into a line of the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generated {
    pub generated: FileId,
    pub original: FileId,
    pub map: PathBuf,
}

/// Interns source paths so a [`Span`] stays 16 bytes.
///
/// Lookup is a linear scan: a design has tens to hundreds of files, and this
/// keeps the serialised form a plain array of paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileTable {
    paths: Vec<PathBuf>,
}

impl FileTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the id for `path`, adding it if unseen.
    ///
    /// Callers should hand in an already-canonicalised path (see
    /// `dunce::canonicalize`) so the same file interned via two spellings does
    /// not produce two ids.
    pub fn intern(&mut self, path: impl Into<PathBuf>) -> FileId {
        let path = path.into();
        if let Some(pos) = self.paths.iter().position(|p| *p == path) {
            return FileId(pos as u32);
        }
        let id = u32::try_from(self.paths.len()).expect("file table exceeded u32::MAX entries");
        self.paths.push(path);
        FileId(id)
    }

    pub fn path(&self, id: FileId) -> Option<&Path> {
        self.paths.get(id.0 as usize).map(PathBuf::as_path)
    }

    /// Renders `path:line:col`, falling back to `<unknown>` for placeholders.
    pub fn render(&self, span: Span) -> String {
        match self.path(span.file) {
            Some(path) => format!("{}:{}:{}", path.display(), span.line, span.col),
            None => "<unknown>".to_string(),
        }
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (FileId, &Path)> {
        self.paths.iter().enumerate().map(|(i, p)| (FileId(i as u32), p.as_path()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_the_same_path_twice_reuses_the_id() {
        let mut table = FileTable::new();
        let a = table.intern("a.sv");
        let b = table.intern("b.sv");
        let a2 = table.intern("a.sv");
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn unknown_span_is_detectable() {
        assert!(Span::UNKNOWN.is_unknown());
        assert!(!Span::new(FileId(0), 1, 1, 3).is_unknown());
    }
}
