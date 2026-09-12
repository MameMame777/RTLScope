//! Diagnostics.
//!
//! D2 of the design spec forbids the tool from being quietly wrong: anything
//! outside the synthesisable subset is skipped *and reported*. Every skip that
//! happens during parsing or elaboration lands here with a machine-readable
//! code, so the CLI, the GUI badge and the future MCP server all describe the
//! same gap in the same words.

use serde::{Deserialize, Serialize};

use crate::span::{FileTable, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The design could not be built, or would be structurally wrong.
    Error,
    /// Something was skipped or guessed; the design is usable but incomplete.
    Warning,
    /// Informational — a heuristic fired, a default was chosen.
    Info,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable, machine-readable diagnostic identity.
///
/// The numeric ids are a public contract: they end up in `--diag-format json`
/// and in MCP responses, so existing codes are never renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DiagCode {
    // 01xx — reading source
    ParseFailed,
    FileUnreadable,
    UnsupportedConstruct,
    UnsupportedExpr,
    ImplicitNet,
    DuplicateModule,
    /// A declaration names a type RTLScope does not know — a `typedef`, an enum,
    /// a struct. Its width is therefore unknown.
    UnknownDataType,
    /// A source written in another language could not be turned into the
    /// SystemVerilog this reads — the transpiler refused it, or is not
    /// installed and left nothing behind from an earlier run.
    TranspileFailed,

    // 02xx — elaboration
    TopNotFound,
    TopAmbiguous,
    ModuleNotFound,
    ParamNotConstant,
    ParamUnknown,
    DefparamUnsupported,
    GenerateLimitExceeded,
    DivisionByZero,
    RecursionLimitExceeded,
    /// An `import` names a package that is in none of the files read.
    PackageNotFound,

    // 03xx — instance connections
    PortNotFound,
    PortUnconnected,
    TooManyConnections,
    WildcardNoMatch,
    DuplicateConnection,

    // 04xx — widths and structure
    WidthMismatch,
    MultipleDrivers,
    LatchInferred,
    ResetNotRecognised,
    ClockNotRecognised,

    // 09xx — internal
    InvariantViolation,
}

impl DiagCode {
    /// Stable printable id, e.g. `RK0203`.
    pub const fn id(self) -> &'static str {
        match self {
            DiagCode::ParseFailed => "RK0101",
            DiagCode::FileUnreadable => "RK0102",
            DiagCode::UnsupportedConstruct => "RK0103",
            DiagCode::UnsupportedExpr => "RK0104",
            DiagCode::ImplicitNet => "RK0105",
            DiagCode::DuplicateModule => "RK0106",
            DiagCode::UnknownDataType => "RK0107",
            DiagCode::TranspileFailed => "RK0108",

            DiagCode::TopNotFound => "RK0201",
            DiagCode::TopAmbiguous => "RK0202",
            DiagCode::ModuleNotFound => "RK0203",
            DiagCode::ParamNotConstant => "RK0204",
            DiagCode::ParamUnknown => "RK0205",
            DiagCode::DefparamUnsupported => "RK0206",
            DiagCode::GenerateLimitExceeded => "RK0207",
            DiagCode::DivisionByZero => "RK0208",
            DiagCode::RecursionLimitExceeded => "RK0209",
            DiagCode::PackageNotFound => "RK0210",

            DiagCode::PortNotFound => "RK0301",
            DiagCode::PortUnconnected => "RK0302",
            DiagCode::TooManyConnections => "RK0303",
            DiagCode::WildcardNoMatch => "RK0304",
            DiagCode::DuplicateConnection => "RK0305",

            DiagCode::WidthMismatch => "RK0401",
            DiagCode::MultipleDrivers => "RK0402",
            DiagCode::LatchInferred => "RK0403",
            DiagCode::ResetNotRecognised => "RK0404",
            DiagCode::ClockNotRecognised => "RK0405",

            DiagCode::InvariantViolation => "RK0901",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diag {
    pub severity: Severity,
    pub code: DiagCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

impl Diag {
    pub fn new(severity: Severity, code: DiagCode, message: impl Into<String>) -> Self {
        Self { severity, code, message: message.into(), span: None }
    }

    pub fn error(code: DiagCode, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, message)
    }

    pub fn warning(code: DiagCode, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, message)
    }

    pub fn info(code: DiagCode, message: impl Into<String>) -> Self {
        Self::new(Severity::Info, code, message)
    }

    /// Attach a source location. Chainable: `Diag::warning(..).at(span)`.
    #[must_use]
    pub fn at(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Diagnostics {
    items: Vec<Diag>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, diag: Diag) {
        self.items.push(diag);
    }

    pub fn extend(&mut self, other: Diagnostics) {
        self.items.extend(other.items);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diag> {
        self.items.iter()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn count(&self, severity: Severity) -> usize {
        self.items.iter().filter(|d| d.severity == severity).count()
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity == Severity::Error)
    }

    /// Human-readable rendering, one diagnostic per two lines:
    ///
    /// ```text
    /// warning[RK0103]: unsupported construct 'initial' skipped
    ///   --> tests/fixtures/unsupported.sv:42:3
    /// ```
    pub fn render(&self, files: &FileTable) -> String {
        let mut out = String::new();
        for diag in &self.items {
            out.push_str(&format!("{}[{}]: {}\n", diag.severity, diag.code.id(), diag.message));
            if let Some(span) = diag.span {
                out.push_str(&format!("  --> {}\n", files.render(span)));
            }
        }
        out
    }
}

impl IntoIterator for Diagnostics {
    type Item = Diag;
    type IntoIter = std::vec::IntoIter<Diag>;
    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

impl FromIterator<Diag> for Diagnostics {
    fn from_iter<I: IntoIterator<Item = Diag>>(iter: I) -> Self {
        Self { items: iter.into_iter().collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_code_and_location() {
        let mut files = FileTable::new();
        let file = files.intern("fixtures/unsupported.sv");
        let mut diags = Diagnostics::new();
        diags.push(
            Diag::warning(
                DiagCode::UnsupportedConstruct,
                "unsupported construct 'initial' skipped",
            )
            .at(Span::new(file, 42, 3, 7)),
        );
        let text = diags.render(&files);
        assert!(text.contains("warning[RK0103]"), "{text}");
        assert!(text.contains("unsupported.sv:42:3"), "{text}");
    }

    #[test]
    fn errors_are_distinguished_from_warnings() {
        let mut diags = Diagnostics::new();
        diags.push(Diag::warning(DiagCode::WidthMismatch, "w"));
        assert!(!diags.has_errors());
        diags.push(Diag::error(DiagCode::TopNotFound, "e"));
        assert!(diags.has_errors());
        assert_eq!(diags.count(Severity::Warning), 1);
        assert_eq!(diags.count(Severity::Error), 1);
    }

    #[test]
    fn diag_codes_are_unique() {
        use DiagCode::*;
        let all = [
            ParseFailed,
            UnknownDataType,
            FileUnreadable,
            UnsupportedConstruct,
            UnsupportedExpr,
            ImplicitNet,
            DuplicateModule,
            TopNotFound,
            TopAmbiguous,
            ModuleNotFound,
            ParamNotConstant,
            ParamUnknown,
            DefparamUnsupported,
            GenerateLimitExceeded,
            DivisionByZero,
            RecursionLimitExceeded,
            PackageNotFound,
            PortNotFound,
            PortUnconnected,
            TooManyConnections,
            WildcardNoMatch,
            DuplicateConnection,
            WidthMismatch,
            MultipleDrivers,
            LatchInferred,
            ResetNotRecognised,
            ClockNotRecognised,
            InvariantViolation,
        ];
        let mut ids: Vec<&str> = all.iter().map(|c| c.id()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate diagnostic id");
    }
}
