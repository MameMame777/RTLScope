//! The elaborated intermediate representation.
//!
//! Three invariants hold for any `Design` handed to a consumer. They are
//! enforced by `rtlscope-elab::validate`, not by this crate — this crate is
//! vocabulary only.
//!
//! 1. Every [`Net`], [`Process`] and [`Instance`] carries a real [`Span`].
//! 2. [`Process::reads`] and [`Process::writes`] are exactly what a traversal
//!    of [`Process::body`] yields. `body` exists for display and for FSM
//!    extraction; the two vectors are what the dataflow graph is built from.
//! 3. No parameter expression survives. Every width and constant is evaluated,
//!    and anything that could not be evaluated was reported and skipped.
//!
//! Enums serialise with an internal `"kind"` tag, which is why every payload
//! variant is a struct variant: the JSON is self-describing for the MCP server
//! that Phase 6 puts in front of it.

use serde::{Deserialize, Serialize};

use crate::arena::{Arena, Idx};
use crate::span::{FileTable, Generated, Span};

pub type ModuleId = Idx<Module>;
pub type NetId = Idx<Net>;

/// Index of a [`Port`] within its [`Module::ports`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PortId(pub u32);

/// Index of an [`Instance`] within its [`Module::insts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstId(pub u32);

/// Index of a [`Process`] within its [`Module::procs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcId(pub u32);

// ---------------------------------------------------------------- design ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Design {
    pub modules: Arena<Module>,
    pub top: ModuleId,
    pub files: FileTable,
    /// Files in `files` that a tool wrote, paired with what it wrote them from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated: Vec<Generated>,
}

impl Design {
    pub fn module(&self, id: ModuleId) -> &Module {
        &self.modules[id]
    }

    pub fn top_module(&self) -> &Module {
        &self.modules[self.top]
    }

    /// After elaboration a module name may be specialised (`fifo$W=8`), so this
    /// matches on the exact stored name.
    pub fn module_by_name(&self, name: &str) -> Option<(ModuleId, &Module)> {
        // The name read first, then the name written: `Control` finds
        // `lights_Control` when nothing is called `Control` outright.
        self.modules
            .iter_enumerated()
            .find(|(_, m)| m.name == name)
            .or_else(|| self.modules.iter_enumerated().find(|(_, m)| m.shown() == name))
    }
}

/// One elaborated module. Distinct parameter bindings produce distinct
/// `Module`s, so nothing here is conditional on a parameter any more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Module {
    pub name: String,
    /// Source name before parameter specialisation, for display and grouping.
    pub base_name: String,
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
    /// Evaluated constants only.
    pub params: Vec<Param>,
    pub ports: Vec<Port>,
    pub nets: Arena<Net>,
    pub insts: Vec<Instance>,
    pub procs: Vec<Process>,
    /// Constructs inside this module that RTLScope does not understand.
    ///
    /// Kept in the IR, not just in the diagnostic stream, so the GUI can badge
    /// the module and the user knows the picture is incomplete (D2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
    /// True when only the module header was found — an IP stub or a missing
    /// file. Drawn as an opaque box rather than silently omitted.
    #[serde(default)]
    pub is_blackbox: bool,
    /// The enumerations declared in this module, with their members' values.
    ///
    /// Elaboration folds an enum member to its number like any other constant,
    /// and the number on its own is not enough to read a state machine back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enums: Vec<EnumType>,
    /// The interface ports, each unfolded into the ports in `ports`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundles: Vec<Bundle>,
    /// The interfaces instantiated inside this module, each unfolded into
    /// the nets in `nets`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iface_insts: Vec<IfaceInstance>,
    pub span: Span,
}

/// An interface port, unfolded: `bus_if.slave s` became the ports `s.data`,
/// `s.valid`, ... — one per signal the modport lists, with the modport's
/// direction. The bundle remembers what they were, so a diagram can draw one
/// pin and a report can say `s : bus_if.slave`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    /// The interface, by the name it was read as.
    pub interface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modport: Option<String>,
    /// The ports this bundle unfolded into, in the interface's order.
    pub ports: Vec<PortId>,
    pub span: Span,
}

/// An interface instantiated inside a module: `bus_if b ();` became the nets
/// `b.data`, `b.valid`, ... which the module's own logic and its children's
/// bundles are wired to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IfaceInstance {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    pub interface: String,
    pub nets: Vec<NetId>,
    pub span: Span,
}

impl Bundle {
    pub fn shown(&self) -> &str {
        self.written.as_deref().unwrap_or(&self.name)
    }
}

impl IfaceInstance {
    pub fn shown(&self) -> &str {
        self.written.as_deref().unwrap_or(&self.name)
    }
}

/// A `typedef enum` and what its members stand for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumType {
    pub name: String,
    pub width: u32,
    /// In declaration order, which is the order the values were assigned in.
    pub members: Vec<EnumMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumMember {
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
    pub value: i64,
    pub span: Span,
}

impl Module {
    /// The name a reader knows this module by.
    ///
    /// The written name when there is one, carrying whatever the elaborator
    /// appended to tell one specialisation from another — `Fifo$W=16` reads as
    /// `Fifo$W=16` whichever language it was written in.
    pub fn shown(&self) -> std::borrow::Cow<'_, str> {
        match &self.written {
            Some(written) => {
                let suffix = self.name.strip_prefix(self.base_name.as_str()).unwrap_or("");
                std::borrow::Cow::Owned(format!("{written}{suffix}"))
            }
            None => std::borrow::Cow::Borrowed(&self.name),
        }
    }
}

impl EnumMember {
    /// The name a reader knows this by: the one written, or the one read.
    pub fn shown(&self) -> &str {
        self.written.as_deref().unwrap_or(&self.name)
    }
}

impl Param {
    /// The name a reader knows this by: the one written, or the one read.
    pub fn shown(&self) -> &str {
        self.written.as_deref().unwrap_or(&self.name)
    }
}

impl Port {
    /// The name a reader knows this by: the one written, or the one read.
    pub fn shown(&self) -> &str {
        self.written.as_deref().unwrap_or(&self.name)
    }
}

impl Net {
    /// The name a reader knows this by: the one written, or the one read.
    pub fn shown(&self) -> &str {
        self.written.as_deref().unwrap_or(&self.name)
    }
}

impl Instance {
    /// The name a reader knows this by: the one written, or the one read.
    pub fn shown(&self) -> &str {
        self.written.as_deref().unwrap_or(&self.name)
    }
}

impl EnumType {
    /// The member with this value, if exactly one has it.
    ///
    /// Two members sharing a value is legal and means the name is a matter of
    /// taste rather than of fact, so neither is offered.
    pub fn name_of(&self, value: i64) -> Option<&str> {
        self.member_of(value).map(|member| member.name.as_str())
    }

    /// The member with this value, if exactly one has it.
    pub fn member_of(&self, value: i64) -> Option<&EnumMember> {
        let mut found = None;
        for member in &self.members {
            if member.value == value {
                if found.is_some() {
                    return None;
                }
                found = Some(member);
            }
        }
        found
    }
}

impl Module {
    /// An expression written out the way the source had it.
    ///
    /// Net names come back, and a constant that an enum in this module has a
    /// name for is printed by that name — which is the difference between a
    /// state diagram someone can check against the code and one they cannot.
    pub fn render(&self, expr: &Expr) -> String {
        match &expr.kind {
            ExprKind::Ref { net } => self.render_ref(net),
            ExprKind::Lit { value } => self.render_const(value),
            ExprKind::Unary { op, operand } => {
                format!("{}{}", op.symbol(), self.render(operand))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // `state == 2'd3` is how the IR holds it and `state == ST_ACK`
                // is what was written; the comparison is where the enum a
                // constant belongs to can be worked out with certainty.
                let (lhs, rhs) = match (self.compared_type(lhs), self.compared_type(rhs)) {
                    (Some(type_name), _) => (self.render(lhs), self.render_as(rhs, type_name)),
                    (_, Some(type_name)) => (self.render_as(lhs, type_name), self.render(rhs)),
                    _ => (self.render(lhs), self.render(rhs)),
                };
                format!("({lhs} {} {rhs})", op.symbol())
            }
            ExprKind::Ternary { cond, then_value, else_value } => format!(
                "({} ? {} : {})",
                self.render(cond),
                self.render(then_value),
                self.render(else_value)
            ),
            ExprKind::Concat { parts } => {
                let parts: Vec<String> = parts.iter().map(|p| self.render(p)).collect();
                format!("{{{}}}", parts.join(", "))
            }
            ExprKind::Repl { count, value } => {
                format!("{{{count}{{{}}}}}", self.render(value))
            }
            ExprKind::DynIndex { net, index } => {
                format!("{}[{}]", self.nets[*net].name, self.render(index))
            }
            ExprKind::Unsupported { text } => text.clone(),
        }
    }

    /// The user-defined type of a plain signal reference, if it has one.
    fn compared_type(&self, expr: &Expr) -> Option<&str> {
        let ExprKind::Ref { net: NetRef::Full { net } } = &expr.kind else { return None };
        self.nets[*net].type_name.as_deref()
    }

    /// A constant printed by the name that enum gives it, when it has one.
    fn render_as(&self, expr: &Expr, type_name: &str) -> String {
        let value = match &expr.kind {
            ExprKind::Lit { value } => value,
            ExprKind::Ref { net: NetRef::Const { value } } => value,
            _ => return self.render(expr),
        };
        match value.to_u64().and_then(|v| i64::try_from(v).ok()) {
            Some(value) => match self.enum_name(Some(type_name), value) {
                Some(name) => name.to_string(),
                None => self.render(expr),
            },
            None => self.render(expr),
        }
    }

    pub fn render_ref(&self, net_ref: &NetRef) -> String {
        match net_ref {
            NetRef::Full { net } => self.nets[*net].name.clone(),
            NetRef::Slice { net, msb, lsb } => {
                let name = &self.nets[*net].name;
                if msb == lsb { format!("{name}[{msb}]") } else { format!("{name}[{msb}:{lsb}]") }
            }
            NetRef::Const { value } => self.render_const(value),
        }
    }

    fn render_const(&self, value: &ConstBits) -> String {
        match value.to_u64() {
            Some(v) => format!("{}'d{v}", value.width),
            None => format!("{}'h<wide>", value.width),
        }
    }

    /// The name an enum in this module gives a value, when one does.
    ///
    /// `type_name` narrows the search to the enum the signal was declared with;
    /// without it, a value that exactly one enum in the module names is taken,
    /// and an ambiguous one is left as a number rather than guessed at.
    pub fn enum_name(&self, type_name: Option<&str>, value: i64) -> Option<&str> {
        self.enum_member(type_name, value).map(|member| member.name.as_str())
    }

    /// The same member, by the name the author wrote — `Idle` for what Veryl
    /// spelled `State_Idle`. What a diagram or a report shows.
    pub fn enum_shown(&self, type_name: Option<&str>, value: i64) -> Option<&str> {
        self.enum_member(type_name, value).map(EnumMember::shown)
    }

    /// The member an enum-typed value stands for, when exactly one does.
    pub fn enum_member(&self, type_name: Option<&str>, value: i64) -> Option<&EnumMember> {
        if let Some(type_name) = type_name {
            let named = self.enums.iter().find(|e| e.name == type_name)?;
            return named.member_of(value);
        }
        let mut found = None;
        for enumeration in &self.enums {
            if let Some(member) = enumeration.member_of(value) {
                if found.is_some() {
                    return None;
                }
                found = Some(member);
            }
        }
        found
    }

    pub fn net(&self, id: NetId) -> &Net {
        &self.nets[id]
    }

    pub fn port(&self, id: PortId) -> &Port {
        &self.ports[id.0 as usize]
    }

    pub fn port_by_name(&self, name: &str) -> Option<(PortId, &Port)> {
        self.ports
            .iter()
            .enumerate()
            .find(|(_, p)| p.name == name)
            .map(|(i, p)| (PortId(i as u32), p))
    }

    pub fn net_by_name(&self, name: &str) -> Option<(NetId, &Net)> {
        self.nets.iter_enumerated().find(|(_, n)| n.name == name)
    }

    pub fn inst(&self, id: InstId) -> &Instance {
        &self.insts[id.0 as usize]
    }

    pub fn proc(&self, id: ProcId) -> &Process {
        &self.procs[id.0 as usize]
    }
}

/// A construct that was recognised but deliberately not modelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub construct: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
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
    /// Integer parameters only — see the D2 subset table.
    pub value: i64,
    /// A `localparam` is derived, not overridable. Both are plain constants
    /// after elaboration, but a caller that wants to re-parameterise a module
    /// needs to know which ones it is allowed to set.
    pub is_local: bool,
    /// The declared width of `parameter logic [5:0] DT`, when it had one.
    ///
    /// A concatenation is the one place this changes the answer — `{VC, DT}`
    /// puts DT in the low six bits only if six is known — so the width has to
    /// survive alongside the value rather than being recomputed later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    pub span: Span,
}

// ----------------------------------------------------------------- ports ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortDir {
    Input,
    Output,
    Inout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
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
    pub dir: PortDir,
    /// The net this port names inside the module.
    pub net: NetId,
    pub span: Span,
}

// ------------------------------------------------------------------ nets ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum NetKind {
    /// A plain vector of `width` bits. `wire`, `logic` and `reg` all land here:
    /// the distinction is a storage hint in SystemVerilog, and what actually
    /// decides flop-vs-wire is the process that drives it.
    Logic,
    /// A one-dimensional unpacked array of `width`-bit words.
    Memory { depth: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Net {
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
    pub width: u32,
    pub kind: NetKind,
    /// The user-defined type it was declared with, when it had one.
    ///
    /// Kept past elaboration because the *width* is not all the type says: a
    /// state register declared `state_e` has names for its values, and a state
    /// diagram that reads `2'd3` where the source says `ST_ACK` is a diagram
    /// nobody can check against the code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    /// True when RTLScope made this net rather than the source declaring it: the
    /// arguments and locals of an inlined function, the wire behind an
    /// expression in a port connection.
    ///
    /// It is real logic and belongs in the diagram, but it is not something the
    /// author can go and change, so anything reporting on *their* code leaves
    /// it out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthesised: bool,
    pub span: Span,
}

// ------------------------------------------------------------- instances ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
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
    pub of: ModuleId,
    /// Resolved connections. Ports left unconnected are absent, and reported
    /// once as `RK0302`.
    pub conns: Vec<Conn>,
    pub span: Span,
}

/// One port of an instance, and what it is tied to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conn {
    pub port: PortId,
    pub net: NetRef,
    /// Where this connection is written.
    ///
    /// A port bound by `.*` has none: nothing was written for it, and the span
    /// is [`Span::UNKNOWN`] to say so.
    pub span: Span,
}

impl Conn {
    pub const fn new(port: PortId, net: NetRef, span: Span) -> Self {
        Self { port, net, span }
    }
}

// ------------------------------------------------------------- processes ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Pos,
    Neg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResetKind {
    /// In the sensitivity list — `always_ff @(posedge clk or negedge rst_n)`.
    Async,
    /// Only in the body — `if (!rst_n) ... else ...`.
    Sync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    High,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reset {
    pub net: NetRef,
    pub kind: ResetKind,
    pub active: Level,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ProcKind {
    /// Edge-triggered. The register-adjacency graph is built from exactly these.
    Ff {
        clk: NetRef,
        edge: Edge,
        rst: Option<Reset>,
    },
    Comb,
    /// Reported as `RK0403` — kept in the IR so it can be shown, not hidden.
    Latch,
    /// Runs before the first clock: the power-on contents of what it writes.
    ///
    /// Not a register-adjacency edge and not combinational logic — a value that
    /// is simply there when the design starts.
    Initial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub kind: ProcKind,
    /// Nets read on a right-hand side or in a condition. Derived from `body`.
    pub reads: Vec<NetRef>,
    /// Nets assigned. Derived from `body`.
    pub writes: Vec<NetRef>,
    pub body: Stmt,
    pub span: Span,
}

// ------------------------------------------------------------ statements ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaseKind {
    Case,
    /// `casez`. `casex` is rejected — it is a synthesis hazard and RTLScope
    /// refuses to guess what it means.
    Casez,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseArm {
    pub labels: Vec<Expr>,
    pub body: Stmt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stmt {
    /// Flattened, so a statement serialises as one object carrying its own
    /// discriminator: `{"stmt":"Assign","lhs":…,"rhs":…,"span":…}`.
    #[serde(flatten)]
    pub kind: StmtKind,
    pub span: Span,
}

impl Stmt {
    pub fn new(kind: StmtKind, span: Span) -> Self {
        Self { kind, span }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stmt")]
pub enum StmtKind {
    Block {
        stmts: Vec<Stmt>,
    },
    Assign {
        lhs: NetRef,
        /// The address of `mem[addr] <= x`.
        ///
        /// A [`NetRef`] cannot hold a non-constant index, but the process still
        /// *reads* that address, and dropping it would leave the dataflow graph
        /// missing an edge into every memory write port.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lhs_index: Option<Box<Expr>>,
        rhs: Expr,
        /// `=` (true) versus `<=` (false).
        blocking: bool,
    },
    If {
        cond: Expr,
        then_branch: Box<Stmt>,
        else_branch: Option<Box<Stmt>>,
    },
    Case {
        subject: Expr,
        case_kind: CaseKind,
        arms: Vec<CaseArm>,
        default: Option<Box<Stmt>>,
    },
    /// A statement RTLScope could not model, kept in place rather than dropped.
    ///
    /// The body then still has the shape the author wrote, with a labelled hole
    /// in it. Dropping the statement would leave a process that reads as
    /// complete while doing less than the RTL does (D2).
    Unsupported {
        construct: String,
    },
}

impl Stmt {
    /// Visit the statements directly under this one, and no deeper.
    ///
    /// The companion to [`Stmt::for_each_stmt`], for walks that need to know
    /// where they are in the tree — a `case` arm's guard, say — rather than
    /// just what is in it.
    pub fn for_each_child<'a>(&'a self, f: &mut impl FnMut(&'a Stmt)) {
        match &self.kind {
            StmtKind::Block { stmts } => stmts.iter().for_each(f),
            StmtKind::Assign { .. } | StmtKind::Unsupported { .. } => {}
            StmtKind::If { then_branch, else_branch, .. } => {
                f(then_branch);
                if let Some(else_branch) = else_branch {
                    f(else_branch);
                }
            }
            StmtKind::Case { arms, default, .. } => {
                arms.iter().for_each(|arm| f(&arm.body));
                if let Some(default) = default {
                    f(default);
                }
            }
        }
    }

    /// Visit this statement and every statement nested under it, outermost first.
    pub fn for_each_stmt(&self, f: &mut impl FnMut(&Stmt)) {
        f(self);
        match &self.kind {
            StmtKind::Block { stmts } => {
                for s in stmts {
                    s.for_each_stmt(f);
                }
            }
            StmtKind::Assign { .. } => {}
            StmtKind::If { then_branch, else_branch, .. } => {
                then_branch.for_each_stmt(f);
                if let Some(e) = else_branch {
                    e.for_each_stmt(f);
                }
            }
            StmtKind::Case { arms, default, .. } => {
                for arm in arms {
                    arm.body.for_each_stmt(f);
                }
                if let Some(d) = default {
                    d.for_each_stmt(f);
                }
            }
            StmtKind::Unsupported { .. } => {}
        }
    }
}

// ----------------------------------------------------------- expressions ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnOp {
    /// `~`
    BitNot,
    /// `!`
    LogNot,
    /// unary `-`
    Neg,
    /// `&`
    RedAnd,
    /// `|`
    RedOr,
    /// `^`
    RedXor,
}

impl UnOp {
    pub fn symbol(self) -> &'static str {
        match self {
            UnOp::BitNot => "~",
            UnOp::LogNot => "!",
            UnOp::Neg => "-",
            UnOp::RedAnd => "&",
            UnOp::RedOr => "|",
            UnOp::RedXor => "^",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
    BitOr,
    BitXor,
    /// `~^` / `^~`
    BitXnor,
    Shl,
    /// `>>` — logical, zero-filling.
    Shr,
    /// `>>>` — arithmetic, sign-filling.
    AShr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LogAnd,
    LogOr,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expr {
    /// Flattened — see [`Stmt::kind`].
    #[serde(flatten)]
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Self { kind, span }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "node")]
pub enum ExprKind {
    Ref {
        net: NetRef,
    },
    Lit {
        value: ConstBits,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Ternary {
        cond: Box<Expr>,
        then_value: Box<Expr>,
        else_value: Box<Expr>,
    },
    Concat {
        parts: Vec<Expr>,
    },
    Repl {
        count: u32,
        value: Box<Expr>,
    },
    /// A non-constant index — `mem[addr]`, `vec[i]`. Constant indices are
    /// folded into a [`NetRef::Slice`] during elaboration.
    DynIndex {
        net: NetId,
        index: Box<Expr>,
    },
    /// An operand outside the subset. Keeping it means the assignment around it
    /// still records its left-hand side, so `writes` stays correct.
    Unsupported {
        text: String,
    },
}

impl Expr {
    /// Visit every [`NetRef`] reachable from this expression.
    ///
    /// Mechanical traversal only. Deciding which of these count as reads is
    /// `rtlscope-elab`'s job, not this crate's.
    pub fn for_each_ref(&self, f: &mut impl FnMut(&NetRef)) {
        match &self.kind {
            ExprKind::Ref { net } => f(net),
            ExprKind::Lit { .. } => {}
            ExprKind::Unary { operand, .. } => operand.for_each_ref(f),
            ExprKind::Binary { lhs, rhs, .. } => {
                lhs.for_each_ref(f);
                rhs.for_each_ref(f);
            }
            ExprKind::Ternary { cond, then_value, else_value } => {
                cond.for_each_ref(f);
                then_value.for_each_ref(f);
                else_value.for_each_ref(f);
            }
            ExprKind::Concat { parts } => {
                for p in parts {
                    p.for_each_ref(f);
                }
            }
            ExprKind::Repl { value, .. } => value.for_each_ref(f),
            ExprKind::DynIndex { net, index } => {
                f(&NetRef::Full { net: *net });
                index.for_each_ref(f);
            }
            ExprKind::Unsupported { .. } => {}
        }
    }
}

// -------------------------------------------------------------- net refs ---

/// A reference to a net, a contiguous slice of one, or a literal.
///
/// Concatenations never appear here: elaboration splits a concatenated
/// connection into anonymous intermediate nets so that every connection is a
/// single `NetRef` and the dataflow graph stays a plain graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum NetRef {
    Full { net: NetId },
    Slice { net: NetId, msb: u32, lsb: u32 },
    Const { value: ConstBits },
}

impl NetRef {
    /// The net this refers to, if it refers to one at all.
    pub fn net_id(&self) -> Option<NetId> {
        match self {
            NetRef::Full { net } | NetRef::Slice { net, .. } => Some(*net),
            NetRef::Const { .. } => None,
        }
    }

    pub fn is_const(&self) -> bool {
        matches!(self, NetRef::Const { .. })
    }
}

/// A sized bit vector constant.
///
/// Backed by 64-bit words rather than a `u64` because this type is frozen and
/// real designs have parameters and literals wider than 64 bits. Bits at or
/// above `width` are always zero.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConstBits {
    pub width: u32,
    /// Least significant word first.
    pub words: Vec<u64>,
}

const fn words_for(width: u32) -> usize {
    (width as usize).div_ceil(64)
}

impl ConstBits {
    pub fn zero(width: u32) -> Self {
        Self { width, words: vec![0; words_for(width)] }
    }

    pub fn from_u64(width: u32, value: u64) -> Self {
        let mut bits = Self::zero(width);
        if let Some(w) = bits.words.first_mut() {
            *w = value;
        }
        bits.mask();
        bits
    }

    /// Builds from raw words, least significant first. Missing words are zero
    /// and bits at or above `width` are cleared, so the result is canonical
    /// however sloppy the input was.
    pub fn from_words(width: u32, mut words: Vec<u64>) -> Self {
        words.resize(words_for(width), 0);
        let mut bits = Self { width, words };
        bits.mask();
        bits
    }

    /// Sign-extends `value` across the full width before masking.
    pub fn from_i64(width: u32, value: i64) -> Self {
        let fill = if value < 0 { u64::MAX } else { 0 };
        let mut bits = Self { width, words: vec![fill; words_for(width)] };
        if let Some(w) = bits.words.first_mut() {
            *w = value as u64;
        }
        bits.mask();
        bits
    }

    /// Clears every bit at or above `width`, so equality is structural.
    fn mask(&mut self) {
        let full_words = (self.width / 64) as usize;
        let rem = self.width % 64;
        for (i, word) in self.words.iter_mut().enumerate() {
            if i < full_words {
                continue;
            }
            *word &= if i == full_words && rem != 0 { (1u64 << rem) - 1 } else { 0 };
        }
    }

    /// The value as a `u64`, or `None` if it does not fit.
    pub fn to_u64(&self) -> Option<u64> {
        if self.words.iter().skip(1).any(|w| *w != 0) {
            return None;
        }
        Some(self.words.first().copied().unwrap_or(0))
    }

    pub fn is_zero(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    pub fn bit(&self, index: u32) -> bool {
        if index >= self.width {
            return false;
        }
        let word = (index / 64) as usize;
        let shift = index % 64;
        self.words.get(word).is_some_and(|w| (w >> shift) & 1 == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::FileId;

    fn span() -> Span {
        Span::new(FileId(0), 1, 1, 1)
    }

    #[test]
    fn const_bits_masks_above_width() {
        let bits = ConstBits::from_u64(4, 0xFF);
        assert_eq!(bits.to_u64(), Some(0x0F));
        assert!(bits.bit(3));
        assert!(!bits.bit(4));
    }

    #[test]
    fn const_bits_sign_extends() {
        let bits = ConstBits::from_i64(8, -1);
        assert_eq!(bits.to_u64(), Some(0xFF));
        let wide = ConstBits::from_i64(128, -1);
        assert_eq!(wide.words, vec![u64::MAX, u64::MAX]);
        assert_eq!(wide.to_u64(), None);
    }

    #[test]
    fn const_bits_zero_width_is_empty() {
        let bits = ConstBits::zero(0);
        assert!(bits.words.is_empty());
        assert!(bits.is_zero());
        assert_eq!(bits.to_u64(), Some(0));
    }

    #[test]
    fn for_each_ref_walks_nested_expressions() {
        let a = NetId::from_raw(0);
        let b = NetId::from_raw(1);
        let expr = Expr::new(
            ExprKind::Binary {
                op: BinOp::Add,
                lhs: Box::new(Expr::new(ExprKind::Ref { net: NetRef::Full { net: a } }, span())),
                rhs: Box::new(Expr::new(
                    ExprKind::Unary {
                        op: UnOp::BitNot,
                        operand: Box::new(Expr::new(
                            ExprKind::Ref { net: NetRef::Full { net: b } },
                            span(),
                        )),
                    },
                    span(),
                )),
            },
            span(),
        );
        let mut seen = Vec::new();
        expr.for_each_ref(&mut |r| seen.extend(r.net_id()));
        assert_eq!(seen, vec![a, b]);
    }

    #[test]
    fn for_each_stmt_visits_both_branches() {
        let assign = |n: u32| {
            Stmt::new(
                StmtKind::Assign {
                    lhs: NetRef::Full { net: NetId::from_raw(n) },
                    lhs_index: None,
                    rhs: Expr::new(ExprKind::Lit { value: ConstBits::zero(1) }, span()),
                    blocking: false,
                },
                span(),
            )
        };
        let stmt = Stmt::new(
            StmtKind::If {
                cond: Expr::new(ExprKind::Lit { value: ConstBits::from_u64(1, 1) }, span()),
                then_branch: Box::new(assign(0)),
                else_branch: Some(Box::new(assign(1))),
            },
            span(),
        );
        let mut count = 0;
        stmt.for_each_stmt(&mut |_| count += 1);
        assert_eq!(count, 3, "the if plus both branches");
    }

    #[test]
    fn net_ref_serialises_with_a_kind_tag() {
        let r = NetRef::Slice { net: NetId::from_raw(2), msb: 7, lsb: 0 };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"kind":"Slice","net":2,"msb":7,"lsb":0}"#);
    }
}
