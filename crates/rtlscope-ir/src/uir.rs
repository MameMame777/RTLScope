//! The unresolved intermediate representation.
//!
//! What `rtlscope-sv` produces and `rtlscope-elab` consumes: one module per source
//! declaration, parameters still expressions, widths still expressions,
//! connections still port *names*, `generate` blocks still folded.
//!
//! This is a separate type from [`crate::ir`] rather than a phase parameter on
//! one type, because the two carry genuinely different invariants and a
//! `Phase`-generic tree fights `serde`'s derive. It lives in `rtlscope-ir` so
//! that `rtlscope-elab` can consume it without depending on `sv-parser`.
//!
//! Anything the lowering could not translate is kept as an `Unsupported` node
//! carrying its source text and span, never dropped: elaboration turns those
//! into diagnostics, and D2 forbids failing silently.

use serde::{Deserialize, Serialize};

use crate::ir::{BinOp, CaseKind, ConstBits, Edge, PortDir, UnOp};
use crate::span::{FileTable, Generated, Span};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UDesign {
    pub modules: Vec<UModule>,
    /// Every `package` in the files, in the order they were read.
    ///
    /// A package is not hardware: it holds constants, types and functions for
    /// modules to share. Elaboration evaluates each once and hands the names
    /// to every module that imports them or spells them out as `pkg::name`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<UPackage>,
    /// Every `interface` in the files, in the order they were read.
    ///
    /// An interface is a bundle of signals with a name, and a `modport` is a
    /// view of it with directions. Elaboration unfolds both: an instance of
    /// one becomes its signals as nets, a port of one becomes its signals as
    /// ports, and `bus.valid` names the net `bus.valid`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interfaces: Vec<UInterface>,
    pub files: FileTable,
    /// Files in `files` that a tool wrote, paired with what it wrote them from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated: Vec<Generated>,
}

impl UDesign {
    pub fn module_by_name(&self, name: &str) -> Option<&UModule> {
        self.modules
            .iter()
            .find(|m| m.name == name)
            .or_else(|| self.modules.iter().find(|m| m.written.as_deref() == Some(name)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UModule {
    pub name: String,
    /// The name as the author wrote it, when a tool wrote the file this was
    /// read from and gave it another. `None` when the two are the same.
    ///
    /// Veryl prefixes every module with the project and rewrites a reset port
    /// by its convention, so `Control` is read as `lights_Control` and `i_rst`
    /// as `i_rst_n`. The name RTLScope works by is the one in the file it read —
    /// simulators, Yosys and waveforms all know that one — and this is the one
    /// a reader knows. See `shown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    pub params: Vec<UParam>,
    pub ports: Vec<UPort>,
    /// Everything in the module body, in source order. Nets, instances and
    /// processes live here rather than in separate vectors because `generate`
    /// blocks nest them, and elaboration is what flattens that.
    pub items: Vec<UItem>,
    /// True for an ANSI header (`module m (input logic a);`). A non-ANSI header
    /// lists bare names and gets its directions from [`UItem::PortDecl`].
    pub ansi_header: bool,
    /// The `import pkg::*;` and `import pkg::name;` lines, wherever they were
    /// written: in the header, in the body, or inside a generate block. An
    /// import inside a block is hoisted to the module — the block's own names
    /// still shadow it, so nothing an author can write is read wrongly by that.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<UImport>,
    pub span: Span,
}

/// `package defs; ... endpackage`
///
/// The same items a module body holds — parameters, typedefs, functions — with
/// nothing that could be hardware. What a package declares is read by name
/// from the modules that use it, so the items are kept unevaluated here just as
/// a module's are, and elaboration binds them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UPackage {
    pub name: String,
    /// The name as the author wrote it, when a tool renamed the package — see
    /// [`UModule::written`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    /// Packages this one imports, for the names its own items use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<UImport>,
    pub items: Vec<UItem>,
    pub span: Span,
}

/// `interface bus_if #(parameter W = 8); logic [W-1:0] data; modport slave
/// (input data); endinterface`
///
/// The same shape as a module — parameters, ports, a body of items — plus the
/// modports. Its body may hold logic of its own, which is instantiated with
/// the interface, as a generate block would be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UInterface {
    pub name: String,
    /// The name as the author wrote it, when a tool renamed the interface —
    /// see [`UModule::written`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    pub params: Vec<UParam>,
    /// The interface's own ports: the `clk` of `interface bus_if (input clk);`,
    /// connected where the interface is instantiated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<UPort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<UImport>,
    pub items: Vec<UItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modports: Vec<UModport>,
    pub span: Span,
}

/// `modport slave (input data, output ready);`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UModport {
    pub name: String,
    pub members: Vec<UModportMember>,
    pub span: Span,
}

/// One signal of a modport, with the direction the modport gives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UModportMember {
    pub name: String,
    pub dir: PortDir,
    pub span: Span,
}

/// What an interface port is a port *of*: `bus_if.slave s` names the
/// interface and the modport, `interface s` names neither and takes whatever
/// is connected, `interface.slave s` names only the modport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UIfacePort {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modport: Option<String>,
}

/// One `import pkg::*;` or `import pkg::name;`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UImport {
    pub package: String,
    /// The one name imported, or `None` for `*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    pub span: Span,
}

// ------------------------------------------------------------ declarations ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UParam {
    pub name: String,
    /// The name as the author wrote it, when a tool wrote the file this was
    /// read from and gave it another. `None` when the two are the same.
    ///
    /// Veryl prefixes every module with the project and rewrites a reset port
    /// by its convention, so `Control` is read as `lights_Control` and `i_rst`
    /// as `i_rst_n`. The name RTLScope works by is the one in the file it read —
    /// simulators, Yosys and waveforms all know that one — and this is the one
    /// a reader knows. See `shown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    /// `localparam` cannot be overridden by an instantiation.
    pub is_local: bool,
    /// The declared range of `parameter logic [5:0] DT`.
    ///
    /// Needed because a concatenation is the one place a constant's *width*
    /// changes its value: `{VC, DT}` puts DT in the low six bits only if six is
    /// known. Dropping the range made every such localparam unevaluable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packed: Option<URange>,
    pub default: Option<UExpr>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UNetType {
    Logic,
    Wire,
    Reg,
    Bit,
    Integer,
}

impl UNetType {
    /// The width the type carries on its own, for the types that have one.
    ///
    /// `int` is 32 bits whether or not anyone writes a range, and treating it
    /// as one bit — which is what a missing range would otherwise mean — makes
    /// every loop counter and function argument declared this way wrong.
    pub fn intrinsic_width(self) -> Option<u32> {
        match self {
            UNetType::Integer => Some(32),
            UNetType::Logic | UNetType::Wire | UNetType::Reg | UNetType::Bit => None,
        }
    }
}

/// `[msb:lsb]`, both still expressions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct URange {
    pub msb: UExpr,
    pub lsb: UExpr,
}

/// A function declared inside a module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UFunction {
    pub name: String,
    /// A task is a function with no result, called as a statement.
    ///
    /// The two are the same construct as far as hardware is concerned — a body
    /// copied to each call site — so they share a shape here, and differ only in
    /// whether anything comes back and how the caller writes the call.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_task: bool,
    /// The return type's range.
    pub packed: Option<URange>,
    /// The return type when it has no range: `function automatic int f`.
    pub net_type: Option<UNetType>,
    /// A user-defined return type — see [`UNet::type_name`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    pub args: Vec<UFunctionArg>,
    /// Variables declared inside the function body.
    pub locals: Vec<UNet>,
    pub body: UStmt,
    /// The package this was declared in, for a function that was. Its body
    /// names the package's own constants without a prefix, and those have to
    /// be in reach when the body is copied into a module that never imported
    /// them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    pub span: Span,
}

/// One argument of a function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UFunctionArg {
    pub name: String,
    /// `output` and `inout` arguments are copied back to the caller when the
    /// body has run, which is how a task returns anything at all.
    pub dir: PortDir,
    pub packed: Option<URange>,
    /// `input int idx` has no range, and is not one bit.
    pub net_type: Option<UNetType>,
    /// A user-defined argument type — see [`UNet::type_name`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    pub span: Span,
}

/// One member of an enum: `IDLE = 2'd0`, or just `IDLE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UEnumMember {
    pub name: String,
    /// The name as the author wrote it, when a tool wrote the file this was
    /// read from and gave it another. `None` when the two are the same.
    ///
    /// Veryl prefixes every module with the project and rewrites a reset port
    /// by its convention, so `Control` is read as `lights_Control` and `i_rst`
    /// as `i_rst_n`. The name RTLScope works by is the one in the file it read —
    /// simulators, Yosys and waveforms all know that one — and this is the one
    /// a reader knows. See `shown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    /// `None` means one more than the member before it, or zero for the first.
    pub value: Option<UExpr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UNet {
    pub name: String,
    /// The name as the author wrote it, when a tool wrote the file this was
    /// read from and gave it another. `None` when the two are the same.
    ///
    /// Veryl prefixes every module with the project and rewrites a reset port
    /// by its convention, so `Control` is read as `lights_Control` and `i_rst`
    /// as `i_rst_n`. The name RTLScope works by is the one in the file it read —
    /// simulators, Yosys and waveforms all know that one — and this is the one
    /// a reader knows. See `shown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    pub net_type: UNetType,
    /// The name of a user-defined type, when the declaration used one.
    ///
    /// `state_e state;` cannot be sized from `net_type` alone — the width lives
    /// in the `typedef`. Keeping the name is what lets elaboration look it up
    /// instead of guessing one bit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    /// `logic [7:0] x` — the vector dimension.
    pub packed: Option<URange>,
    /// `logic x [0:255]` — the array dimension, which makes this a memory.
    pub unpacked: Option<URange>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UPort {
    pub name: String,
    /// The name as the author wrote it, when a tool wrote the file this was
    /// read from and gave it another. `None` when the two are the same.
    ///
    /// Veryl prefixes every module with the project and rewrites a reset port
    /// by its convention, so `Control` is read as `lights_Control` and `i_rst`
    /// as `i_rst_n`. The name RTLScope works by is the one in the file it read —
    /// simulators, Yosys and waveforms all know that one — and this is the one
    /// a reader knows. See `shown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    /// `None` in a non-ANSI header until the body declaration is merged in.
    pub dir: Option<PortDir>,
    pub net_type: Option<UNetType>,
    /// A user-defined type name — see [`UNet::type_name`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    pub packed: Option<URange>,
    pub unpacked: Option<URange>,
    /// Set when the port is a whole interface rather than a wire. Such a port
    /// has no direction, type or width of its own; elaboration unfolds it
    /// into the interface's signals, one port each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iface: Option<UIfacePort>,
    pub span: Span,
}

// --------------------------------------------------------------- instances ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum UParamOverrides {
    Empty,
    Positional { values: Vec<UExpr> },
    Named { values: Vec<(String, UExpr)> },
}

/// One named port connection. `expr` is `None` for `.port()`, which explicitly
/// leaves a port open; `.port` shorthand is expanded to `Some(Ident(port))`
/// during lowering so elaboration sees only one form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UConn {
    pub port: String,
    pub expr: Option<UExpr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum UConns {
    Empty,
    Positional {
        values: Vec<Option<UExpr>>,
    },
    Named {
        conns: Vec<UConn>,
    },
    /// `.*` — every remaining port binds to a same-named net. `conns` holds the
    /// connections written explicitly alongside it, which take precedence.
    Wildcard {
        conns: Vec<UConn>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UInstance {
    pub module_name: String,
    pub name: String,
    /// The name as the author wrote it, when a tool wrote the file this was
    /// read from and gave it another. `None` when the two are the same.
    ///
    /// Veryl prefixes every module with the project and rewrites a reset port
    /// by its convention, so `Control` is read as `lights_Control` and `i_rst`
    /// as `i_rst_n`. The name RTLScope works by is the one in the file it read —
    /// simulators, Yosys and waveforms all know that one — and this is the one
    /// a reader knows. See `shown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    pub param_overrides: UParamOverrides,
    pub conns: UConns,
    pub span: Span,
}

// ------------------------------------------------------------------- items ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UEvent {
    /// `None` for a level-sensitive term in an `always @(a or b)` list.
    pub edge: Option<Edge>,
    pub expr: UExpr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum UProcKind {
    AlwaysFf {
        events: Vec<UEvent>,
    },
    AlwaysComb,
    AlwaysLatch,
    /// `initial begin rom[0] = ...; end`
    ///
    /// In simulation this runs once at time zero; on an FPGA it is the power-on
    /// contents of the memory or register it writes, which the bitstream
    /// carries. Either way it describes state that exists before the first
    /// clock, and dropping it loses a ROM's entire contents.
    Initial,
    /// Plain `always @(...)`. Classified into flop or combinational logic during
    /// elaboration by looking at whether the events carry edges.
    AlwaysAt {
        events: Vec<UEvent>,
        star: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "item")]
pub enum UItem {
    Net {
        net: UNet,
    },
    Inst {
        inst: UInstance,
    },
    Param {
        param: UParam,
    },
    /// Non-ANSI body declaration that gives a header port its direction.
    PortDecl {
        name: String,
        dir: PortDir,
        net_type: Option<UNetType>,
        packed: Option<URange>,
        unpacked: Option<URange>,
        span: Span,
    },
    Assign {
        lhs: UExpr,
        rhs: UExpr,
        span: Span,
    },
    Proc {
        kind: UProcKind,
        body: UStmt,
        span: Span,
    },
    GenerateFor {
        genvar: String,
        init: UExpr,
        cond: UExpr,
        /// The value assigned back to the genvar each iteration, i.e. the `i+1`
        /// of `i = i + 1`.
        step: UExpr,
        /// The loop body. A labelled `begin : g_tap` arrives as a single
        /// [`UItem::GenerateBlock`], which is what gives the unrolled instances
        /// their `g_tap[0].` prefixes.
        body: Vec<UItem>,
        span: Span,
    },
    /// The two branches carry their own labels, because `if (X) begin : a ...
    /// end else begin : b ... end` names both blocks and elaboration needs
    /// whichever one survives.
    GenerateIf {
        cond: UExpr,
        then_items: Vec<UItem>,
        else_items: Vec<UItem>,
        span: Span,
    },
    GenerateBlock {
        label: Option<String>,
        items: Vec<UItem>,
        span: Span,
    },
    /// `function automatic logic [7:0] sat(input logic [21:0] a); ... endfunction`
    ///
    /// Kept whole rather than turned into logic here, because a function is not
    /// hardware until it is called: each call site becomes its own copy of the
    /// body, which is what synthesis does with it too.
    Function {
        func: UFunction,
    },
    /// `typedef enum logic [1:0] { IDLE, RUN } state_e;`
    ///
    /// Carries two things elaboration needs: the width every variable of this
    /// type gets, and the members, which are *constants in the enclosing
    /// scope*. Without them `IDLE` looks like an undeclared signal and the
    /// design grows a phantom net for every state name.
    TypedefEnum {
        name: String,
        /// The base type's range: the `[1:0]` of `enum logic [1:0]`.
        packed: Option<URange>,
        members: Vec<UEnumMember>,
        span: Span,
    },
    /// `typedef struct packed { logic [7:0] value; logic valid; } beat_t;`
    ///
    /// Each member is shaped like a net declaration, because that is what it
    /// is: a name with a type. What elaboration needs is the width a variable
    /// of this type gets, which is the members' widths added up — a packed
    /// struct is a bit vector with names for its parts.
    TypedefStruct {
        name: String,
        members: Vec<UNet>,
        span: Span,
    },
    /// Recognised and rejected, kept so elaboration can point at the line.
    Defparam {
        text: String,
        span: Span,
    },
    Unsupported {
        construct: String,
        span: Span,
    },
}

impl UItem {
    pub fn span(&self) -> Span {
        match self {
            UItem::Net { net } => net.span,
            UItem::Inst { inst } => inst.span,
            UItem::Param { param } => param.span,
            UItem::Function { func } => func.span,
            UItem::PortDecl { span, .. }
            | UItem::Assign { span, .. }
            | UItem::Proc { span, .. }
            | UItem::GenerateFor { span, .. }
            | UItem::GenerateIf { span, .. }
            | UItem::GenerateBlock { span, .. }
            | UItem::TypedefStruct { span, .. }
            | UItem::TypedefEnum { span, .. }
            | UItem::Defparam { span, .. }
            | UItem::Unsupported { span, .. } => *span,
        }
    }
}

// -------------------------------------------------------------- statements ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UCaseArm {
    pub labels: Vec<UExpr>,
    pub body: UStmt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UStmt {
    #[serde(flatten)]
    pub kind: Box<UStmtKind>,
    pub span: Span,
}

impl UStmt {
    pub fn new(kind: UStmtKind, span: Span) -> Self {
        Self { kind: Box::new(kind), span }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stmt")]
pub enum UStmtKind {
    Block {
        stmts: Vec<UStmt>,
        /// Variables declared by a `begin ... end` block itself.
        ///
        /// `always_comb begin automatic logic v; ... end` gives `v` storage
        /// that belongs to the block, not to the module. Dropping the
        /// declaration does not drop the uses, which then look like undeclared
        /// signals and get inferred as one bit — silently the wrong width.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        decls: Vec<UNet>,
    },
    Assign {
        lhs: UExpr,
        rhs: UExpr,
        blocking: bool,
    },
    If {
        cond: UExpr,
        then_branch: UStmt,
        else_branch: Option<UStmt>,
    },
    Case {
        subject: UExpr,
        case_kind: CaseKind,
        arms: Vec<UCaseArm>,
        default: Option<UStmt>,
    },
    /// `for (int i = 0; i < N; i++) ...` inside a process.
    ///
    /// Unrolled by elaboration exactly as a `generate for` is. The bounds have
    /// to be constant, because the hardware has no loop to run — what the RTL
    /// describes is the unrolled form.
    For {
        /// The loop variable, a constant within each unrolled copy.
        var: String,
        init: UExpr,
        cond: UExpr,
        /// The value the variable takes next, i.e. the `i + 1` of `i++`.
        step: UExpr,
        body: UStmt,
    },
    /// `while (sum >= DEPTH) sum -= DEPTH;`
    ///
    /// Kept as a loop until elaboration, which turns it into the copies of the
    /// body the hardware actually is — one per iteration it could take, each
    /// guarded by the condition.
    While {
        cond: UExpr,
        body: Box<UStmt>,
    },
    /// `load_tx_byte(op, idx, addr, value, byte);` — a task called as a
    /// statement. Elaboration copies the body in, the same as for a function.
    Call {
        name: String,
        args: Vec<UExpr>,
    },
    Unsupported {
        construct: String,
    },
}

// ------------------------------------------------------------- expressions ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UExpr {
    #[serde(flatten)]
    pub kind: Box<UExprKind>,
    pub span: Span,
}

impl UExpr {
    pub fn new(kind: UExprKind, span: Span) -> Self {
        Self { kind: Box::new(kind), span }
    }

    pub fn ident(name: impl Into<String>, span: Span) -> Self {
        Self::new(UExprKind::Ident { name: name.into() }, span)
    }

    pub fn int(value: i64, span: Span) -> Self {
        Self::new(UExprKind::Int { value }, span)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "node")]
pub enum UExprKind {
    /// A bare name: a net, a port, a parameter or a genvar. Which one it is is
    /// not known until elaboration resolves the scope.
    Ident {
        name: String,
    },
    /// An unsized integer literal — `8`, `-1`.
    Int {
        value: i64,
    },
    /// A sized literal — `8'hFF`.
    Sized {
        value: ConstBits,
    },
    /// `'0` or `'1` — a fill literal that takes its width from context.
    /// Idiomatic SystemVerilog resets are written this way, so the subset
    /// cannot skip it. Elaboration turns it into a sized [`ConstBits`] once the
    /// assignment target's width is known.
    Fill {
        ones: bool,
    },
    Unary {
        op: UnOp,
        operand: UExpr,
    },
    Binary {
        op: BinOp,
        lhs: UExpr,
        rhs: UExpr,
    },
    Ternary {
        cond: UExpr,
        then_value: UExpr,
        else_value: UExpr,
    },
    Concat {
        parts: Vec<UExpr>,
    },
    Repl {
        count: UExpr,
        value: UExpr,
    },
    /// `base[index]`.
    Index {
        base: UExpr,
        index: UExpr,
    },
    /// `base[msb:lsb]`.
    Range {
        base: UExpr,
        msb: UExpr,
        lsb: UExpr,
    },
    /// `$clog2(arg)` — the one system function inside the D2 subset, because
    /// width arithmetic is unwritable without it.
    Clog2 {
        arg: UExpr,
    },
    /// `16'(expr)` — a cast that truncates or extends the value to a width.
    Cast {
        width: UExpr,
        value: UExpr,
    },
    /// `sat(a, sh, ab)` — a call to a function declared in the same module.
    ///
    /// Elaboration inlines it at each call site. Kept as a call until then
    /// because the arguments differ per site and the body does not.
    Call {
        name: String,
        args: Vec<UExpr>,
    },
    /// Verbatim source for anything else, so elaboration can report it with the
    /// text the author actually wrote.
    Unsupported {
        text: String,
    },
}

/// Renders an unresolved expression back to something SystemVerilog-shaped.
///
/// Binary and ternary forms are always parenthesised rather than tracking
/// precedence: this is for diagnostics and `dump-ports` output, where being
/// unambiguous matters more than matching the author's punctuation.
impl std::fmt::Display for UExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.kind {
            UExprKind::Ident { name } => f.write_str(name),
            UExprKind::Int { value } => write!(f, "{value}"),
            UExprKind::Sized { value } => match value.to_u64() {
                Some(v) => write!(f, "{}'h{v:x}", value.width),
                None => write!(f, "{}'h<wide>", value.width),
            },
            UExprKind::Fill { ones } => write!(f, "'{}", u8::from(*ones)),
            UExprKind::Unary { op, operand } => write!(f, "{}{operand}", unary_symbol(*op)),
            UExprKind::Binary { op, lhs, rhs } => {
                write!(f, "({lhs} {} {rhs})", binary_symbol(*op))
            }
            UExprKind::Ternary { cond, then_value, else_value } => {
                write!(f, "({cond} ? {then_value} : {else_value})")
            }
            UExprKind::Concat { parts } => {
                f.write_str("{")?;
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{part}")?;
                }
                f.write_str("}")
            }
            UExprKind::Repl { count, value } => write!(f, "{{{count}{value}}}"),
            UExprKind::Index { base, index } => write!(f, "{base}[{index}]"),
            UExprKind::Range { base, msb, lsb } => write!(f, "{base}[{msb}:{lsb}]"),
            UExprKind::Clog2 { arg } => write!(f, "$clog2({arg})"),
            UExprKind::Cast { width, value } => write!(f, "{width}'({value})"),
            UExprKind::Call { name, args } => {
                write!(f, "{name}(")?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                f.write_str(")")
            }
            UExprKind::Unsupported { text } => f.write_str(text),
        }
    }
}

impl std::fmt::Display for URange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}:{}]", self.msb, self.lsb)
    }
}

const fn unary_symbol(op: UnOp) -> &'static str {
    match op {
        UnOp::BitNot => "~",
        UnOp::LogNot => "!",
        UnOp::Neg => "-",
        UnOp::RedAnd => "&",
        UnOp::RedOr => "|",
        UnOp::RedXor => "^",
    }
}

const fn binary_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::BitXnor => "~^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
        BinOp::AShr => ">>>",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::LogAnd => "&&",
        BinOp::LogOr => "||",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::FileId;

    fn span() -> Span {
        Span::new(FileId(0), 3, 5, 2)
    }

    #[test]
    fn expressions_render_unambiguously() {
        let expr = UExpr::new(
            UExprKind::Binary {
                op: BinOp::Sub,
                lhs: UExpr::ident("W", span()),
                rhs: UExpr::int(1, span()),
            },
            span(),
        );
        assert_eq!(expr.to_string(), "(W - 1)");

        let clog2 = UExpr::new(UExprKind::Clog2 { arg: UExpr::ident("DEPTH", span()) }, span());
        assert_eq!(clog2.to_string(), "$clog2(DEPTH)");

        let range = URange { msb: expr, lsb: UExpr::int(0, span()) };
        assert_eq!(range.to_string(), "[(W - 1):0]");
    }

    #[test]
    fn expressions_round_trip_through_json() {
        let expr = UExpr::new(
            UExprKind::Binary {
                op: BinOp::Add,
                lhs: UExpr::ident("W", span()),
                rhs: UExpr::int(1, span()),
            },
            span(),
        );
        let json = serde_json::to_string(&expr).unwrap();
        let back: UExpr = serde_json::from_str(&json).unwrap();
        assert_eq!(back, expr);
    }

    #[test]
    fn item_span_covers_every_variant() {
        let net = UItem::Net {
            net: UNet {
                name: "x".into(),
                written: None,
                net_type: UNetType::Logic,
                type_name: None,
                packed: None,
                unpacked: None,
                span: span(),
            },
        };
        assert_eq!(net.span(), span());
        let unsupported = UItem::Unsupported { construct: "initial".into(), span: span() };
        assert_eq!(unsupported.span(), span());
    }
}
