//! Thin wrapper over `sv_parser::parse_sv`.
//!
//! Two things the wrapper exists for.
//!
//! The error type: `sv_parser::Error::Parse` carries its location as a raw
//! `Option<(PathBuf, usize)>` byte offset, which renders as a Rust tuple if you
//! print it. [`ParseError`] splits the location out so the caller can turn it
//! into a real `file:line:col` span.
//!
//! And the stack, which is why [`parse_file`] must not be called directly from
//! a normal thread — see its documentation and `lower::lower_files`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sv_parser::{Defines, SyntaxTree};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The file is not valid SystemVerilog. `sv-parser` has no error recovery,
    /// so this costs the whole file — the caller keeps going with the others.
    #[error("syntax error")]
    Syntax { path: PathBuf, offset: usize },

    /// Everything else: unreadable file, bad include, macro trouble.
    #[error("{path}: {message}", path = path.display())]
    Other { path: PathBuf, message: String },
}

impl ParseError {
    fn from_sv(path: &Path, error: sv_parser::Error) -> Self {
        match error {
            sv_parser::Error::Parse(Some((path, offset)))
            | sv_parser::Error::Preprocess(Some((path, offset))) => {
                ParseError::Syntax { path, offset }
            }
            other => ParseError::Other { path: path.to_path_buf(), message: other.to_string() },
        }
    }

    /// Where the failure is, when that is known well enough to point at.
    pub fn location(&self) -> Option<(&Path, usize)> {
        match self {
            ParseError::Syntax { path, offset } => Some((path.as_path(), *offset)),
            ParseError::Other { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    /// `-D NAME[=VALUE]`
    pub defines: Vec<(String, Option<String>)>,
    /// `-I DIR`
    pub include_paths: Vec<PathBuf>,
}

pub struct ParsedFile {
    pub path: PathBuf,
    pub tree: SyntaxTree,
    pub defines: Defines,
}

/// Parses one file.
///
/// Must be called with a large stack — see [`crate::lower::lower_files`], which
/// is what provides one. A `SyntaxTree` is far too deeply nested a type to send
/// between threads (the compiler's own `Send` check overflows on it), so the
/// thread has to wrap the whole job rather than just this call.
pub fn parse_file(path: &Path, options: &ParseOptions) -> Result<ParsedFile, ParseError> {
    let mut defines: Defines = HashMap::new();
    for (name, value) in &options.defines {
        let define = value.as_ref().map(|text| sv_parser::Define {
            identifier: name.clone(),
            arguments: Vec::new(),
            text: Some(sv_parser::DefineText { text: text.clone(), origin: None }),
        });
        defines.insert(name.clone(), define);
    }

    let (tree, defines) = sv_parser::parse_sv(path, &defines, &options.include_paths, false, false)
        .map_err(|source| ParseError::from_sv(path, source))?;

    Ok(ParsedFile { path: path.to_path_buf(), tree, defines })
}
