//! Shared vocabulary for RTLScope.
//!
//! Every other crate speaks these types and none of them speak each other's.
//! There is deliberately no analysis logic here: this crate defines what a
//! design *is*, and `rtlscope-sv` / `rtlscope-elab` / `rtlscope-graph` define what is
//! done to it. Structural traversal helpers (`for_each_ref`, `for_each_stmt`)
//! are the one exception — they are accessors, and duplicating them in every
//! consumer would be worse.
//!
//! Two representations live side by side:
//!
//! - [`uir`] — unresolved. Parameters and widths are still expressions,
//!   connections are still port names, `generate` blocks are still folded.
//! - [`ir`] — elaborated. Everything is a constant, every connection points at
//!   a net, and every node carries a source [`span::Span`].
//!
//! The JSON shape of these types is a public contract from the first release:
//! `rtlscope dump-ir` emits it and the Phase 6 MCP server serves it. Snapshot
//! tests pin it so a refactor cannot quietly change the wire format.

pub mod arena;
pub mod diag;
pub mod ir;
pub mod span;
pub mod uir;

pub use arena::{Arena, Idx};
pub use diag::{Diag, DiagCode, Diagnostics, Severity};
pub use ir::{
    BinOp, Bundle, CaseArm, CaseKind, Conn, ConstBits, Design, Edge, EnumMember, EnumType, Expr,
    ExprKind, IfaceInstance, InstId, Instance, Level, Module, ModuleId, Net, NetId, NetKind,
    NetRef, Param, Port, PortDir, PortId, ProcId, ProcKind, Process, Reset, ResetKind, Skipped,
    Stmt, StmtKind, UnOp,
};
pub use span::{FileId, FileTable, Generated, Span};
pub use uir::{
    UCaseArm, UConn, UConns, UDesign, UEnumMember, UEvent, UExpr, UExprKind, UFunction,
    UFunctionArg, UIfacePort, UImport, UInstance, UInterface, UItem, UModport, UModportMember,
    UModule, UNet, UNetType, UPackage, UParam, UParamOverrides, UPort, UProcKind, URange, UStmt,
    UStmtKind,
};
