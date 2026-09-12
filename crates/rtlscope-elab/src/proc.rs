//! Building [`Process`]es: name resolution, read/write derivation, and working
//! out which flop is clocked by what.
//!
//! The classification is where the judgement lives. `always_ff @(posedge clk or
//! negedge rst_n)` names two edges and says nothing about which is the clock —
//! that is convention, not syntax. RTLScope decides by looking at the body: the
//! signal the process tests *first*, before doing anything else, is the reset,
//! and the other edge is the clock. When the body does not settle it, the
//! result is reported rather than assumed, because a mislabelled clock would
//! put the register-adjacency graph — and every pipeline stage derived from it
//! — in the wrong shape.

use rtlscope_ir::{
    ConstBits, Diag, DiagCode, Diagnostics, Edge, Expr, ExprKind, Level, NetId, NetRef, PortDir,
    ProcKind, Process, Reset, ResetKind, Span, Stmt, StmtKind, UCaseArm, UEvent, UExpr, UExprKind,
    UFunction, UNetType, UProcKind, URange, UStmt, UStmtKind,
};

use crate::inline::{Renames, rename_stmt};
use crate::value::{Scope, eval};

/// What the caller must provide to resolve a name.
///
/// Keeps this module independent of how the enclosing module's nets are stored.
pub(crate) trait NameResolver {
    /// The net a name refers to, creating an implicit one-bit net (and saying
    /// so) if it was never declared.
    fn resolve_net(
        &mut self,
        name: &str,
        span: Span,
        diags: &mut Diagnostics,
    ) -> rtlscope_ir::NetId;

    /// Declares a net that the source did not: the arguments, locals and return
    /// value of an inlined function.
    fn declare_local(&mut self, name: &str, width: u32, span: Span) -> NetId;

    /// A function declared in this module, by name.
    ///
    /// Returned by value because the caller goes on to mutate the same resolver
    /// while copying the body, and a function is a few statements at most.
    fn function(&self, name: &str) -> Option<UFunction>;

    /// A number that has not been used for a call site in this module before.
    fn next_call_site(&mut self) -> usize;

    /// The width of a user-defined type declared in this module.
    fn type_width(&self, name: &str) -> Option<u32>;

    /// The constants of a package, for the body of a function declared in it.
    fn package_scope(&self, package: &str) -> Option<Scope>;

    /// The width of a net already declared under this name, without declaring
    /// one for a name that has none.
    fn declared_width_of(&self, name: &str) -> Option<u32>;

    /// Opens a nested name scope, for the declarations of a `begin` block.
    fn push_scope(&mut self);

    /// Closes the scope opened by [`NameResolver::push_scope`].
    fn pop_scope(&mut self);
}

pub(crate) struct ProcBuilder<'a, R: NameResolver> {
    pub scope: &'a Scope,
    pub names: &'a mut R,
    pub diags: &'a mut Diagnostics,
    /// Statements that have to run before the one being built.
    ///
    /// An inlined function call is several statements pretending to be an
    /// expression, so lowering `y = sat(a) + 1` produces logic that has to be
    /// placed *ahead* of the assignment. They collect here and are flushed by
    /// [`ProcBuilder::stmt`], which is the nearest enclosing place a statement
    /// can legally go.
    pub prelude: Vec<Stmt>,
    /// How many function bodies deep this builder already is.
    pub depth: usize,
}

/// How far one call may nest inside another.
///
/// SystemVerilog forbids recursion in a synthesisable function, but nothing
/// stops a file from containing it, and a copy-at-each-call inliner follows it
/// forever. The limit is generous: real helper functions call one or two levels
/// deep.
const MAX_INLINE_DEPTH: usize = 16;

/// How many times one `for` inside a process may unroll.
///
/// The same guard `generate for` has, for the same reason: a condition that is
/// never false would otherwise build statements until memory ran out.
const MAX_UNROLL: i64 = 4096;

/// How many guarded copies a loop with a signal-dependent condition may become.
///
/// `for (idx = 0; idx < amount; idx++)` runs a number of times that is not known
/// until the design is running, but it is not unbounded: `amount` is three bits,
/// so eight copies of the body — each under `if (idx < amount)` — are exactly
/// the hardware, which is how a barrel shifter gets built. The cap is for the
/// cases where the bound is a 32-bit `int` and 2^32 copies would be absurd; a
/// loop that hits it is reported, because past that point the model is short.
const MAX_GUARDED_UNROLL: i64 = 64;

impl<R: NameResolver> ProcBuilder<'_, R> {
    /// A procedural block.
    pub fn process(&mut self, kind: &UProcKind, body: &UStmt, span: Span) -> Process {
        let body = self.stmt(body);
        let kind = self.classify(kind, &body, span);
        let (reads, writes) = derive_reads_writes(&body, &kind);
        Process { kind, reads, writes, body, span }
    }

    /// A continuous assignment, which is combinational logic written without an
    /// `always_comb` around it.
    pub fn continuous_assign(&mut self, lhs: &UExpr, rhs: &UExpr, span: Span) -> Process {
        let (lhs_ref, lhs_index) = self.assign_target(lhs);
        let rhs = self.expr(rhs);
        let assign =
            Stmt::new(StmtKind::Assign { lhs: lhs_ref, lhs_index, rhs, blocking: false }, span);
        // A call on the right-hand side turns one assignment into several.
        let body = if self.prelude.is_empty() {
            assign
        } else {
            let mut stmts = std::mem::take(&mut self.prelude);
            stmts.push(assign);
            Stmt::new(StmtKind::Block { stmts }, span)
        };
        let kind = ProcKind::Comb;
        let (reads, writes) = derive_reads_writes(&body, &kind);
        Process { kind, reads, writes, body, span }
    }

    // ------------------------------------------------------- classification ---

    fn classify(&mut self, kind: &UProcKind, body: &Stmt, span: Span) -> ProcKind {
        match kind {
            UProcKind::AlwaysComb => ProcKind::Comb,
            UProcKind::AlwaysLatch => ProcKind::Latch,
            UProcKind::Initial => ProcKind::Initial,
            UProcKind::AlwaysFf { events } => self.flop(events, body, span, true),
            UProcKind::AlwaysAt { events, star } => {
                // A plain `always` is a flop only if its sensitivity list has an
                // edge in it; `always @(a or b)` and `always @*` are logic.
                if *star || !events.iter().any(|event| event.edge.is_some()) {
                    ProcKind::Comb
                } else {
                    self.flop(events, body, span, false)
                }
            }
        }
    }

    fn flop(&mut self, events: &[UEvent], body: &Stmt, span: Span, declared_ff: bool) -> ProcKind {
        let edges: Vec<(Edge, NetRef)> = events
            .iter()
            .filter_map(|event| {
                let edge = event.edge?;
                Some((edge, self.net_ref(&event.expr)))
            })
            .collect();

        if edges.is_empty() {
            if declared_ff {
                self.diags.push(
                    Diag::warning(
                        DiagCode::ClockNotRecognised,
                        "`always_ff` with no edge in its sensitivity list; treating it as \
                         combinational logic",
                    )
                    .at(span),
                );
            }
            return ProcKind::Comb;
        }

        // The signal the body tests before doing anything else.
        let guard = leading_guard(body);

        match edges.len() {
            1 => {
                let (edge, clk) = edges[0].clone();
                // One edge means one clock. A reset, if there is one, is tested
                // in the body and so is synchronous.
                // Not every leading `if` is a reset. `if (we) regs[a] <= d` is
                // a write enable, and calling it a reset would put a false edge
                // in the register-adjacency graph. What distinguishes a reset is
                // that its branch drives registers to *constants*.
                let rst = guard
                    .filter(|(net, _)| *net != clk && leading_branch_is_constant(body))
                    .map(|(net, active)| Reset { net, kind: ResetKind::Sync, active });
                ProcKind::Ff { clk, edge, rst }
            }
            2 => {
                // Two edges: one clock, one asynchronous reset. The body says
                // which is which.
                let reset_index =
                    guard.as_ref().and_then(|(net, _)| edges.iter().position(|(_, e)| e == net));

                let Some(reset_index) = reset_index else {
                    self.diags.push(
                        Diag::info(
                            DiagCode::ResetNotRecognised,
                            "two edges in the sensitivity list but the body does not test \
                             either one first, so the first is taken as the clock",
                        )
                        .at(span),
                    );
                    let (edge, clk) = edges[0].clone();
                    return ProcKind::Ff { clk, edge, rst: None };
                };

                let clock_index = 1 - reset_index;
                let (edge, clk) = edges[clock_index].clone();
                let (_, reset_net) = edges[reset_index].clone();
                let active = guard.map_or(Level::High, |(_, active)| active);
                ProcKind::Ff {
                    clk,
                    edge,
                    rst: Some(Reset { net: reset_net, kind: ResetKind::Async, active }),
                }
            }
            _ => {
                self.diags.push(
                    Diag::warning(
                        DiagCode::ClockNotRecognised,
                        format!(
                            "{} edges in one sensitivity list; RTLScope models at most a clock \
                             and an asynchronous reset, and took the first as the clock",
                            edges.len()
                        ),
                    )
                    .at(span),
                );
                let (edge, clk) = edges[0].clone();
                ProcKind::Ff { clk, edge, rst: None }
            }
        }
    }

    // ------------------------------------------------------------ statements ---

    /// Lowers one statement, with any logic hoisted out of its expressions
    /// placed in front of it.
    fn stmt(&mut self, stmt: &UStmt) -> Stmt {
        let span = stmt.span;
        let outer = std::mem::take(&mut self.prelude);
        let lowered = self.stmt_inner(stmt);
        let mut stmts = std::mem::replace(&mut self.prelude, outer);
        if stmts.is_empty() {
            return lowered;
        }
        stmts.push(lowered);
        Stmt::new(StmtKind::Block { stmts }, span)
    }

    fn stmt_inner(&mut self, stmt: &UStmt) -> Stmt {
        let span = stmt.span;
        match &*stmt.kind {
            UStmtKind::Block { stmts, decls } => {
                // A block's own declarations are storage that belongs to the
                // block, so they go in a scope of their own: two processes may
                // each declare a `v`, and they are not the same net.
                if !decls.is_empty() {
                    self.names.push_scope();
                    for net in decls {
                        let width = self.width_of(
                            net.packed.as_ref(),
                            Some(net.net_type),
                            net.type_name.as_ref(),
                        );
                        self.names.declare_local(&net.name, width, net.span);
                    }
                }
                let stmts = stmts.iter().map(|s| self.stmt(s)).collect();
                if !decls.is_empty() {
                    self.names.pop_scope();
                }
                Stmt::new(StmtKind::Block { stmts }, span)
            }
            UStmtKind::Assign { lhs, rhs, blocking } => {
                let (lhs, lhs_index) = self.assign_target(lhs);
                let rhs = self.expr(rhs);
                Stmt::new(StmtKind::Assign { lhs, lhs_index, rhs, blocking: *blocking }, span)
            }
            UStmtKind::If { cond, then_branch, else_branch } => {
                let cond = self.expr(cond);
                let then_branch = Box::new(self.stmt(then_branch));
                let else_branch = else_branch.as_ref().map(|s| Box::new(self.stmt(s)));
                Stmt::new(StmtKind::If { cond, then_branch, else_branch }, span)
            }
            UStmtKind::Case { subject, case_kind, arms, default } => {
                let subject = self.expr(subject);
                let arms = arms
                    .iter()
                    .map(|UCaseArm { labels, body }| rtlscope_ir::CaseArm {
                        labels: labels.iter().map(|label| self.expr(label)).collect(),
                        body: self.stmt(body),
                    })
                    .collect();
                let default = default.as_ref().map(|s| Box::new(self.stmt(s)));
                Stmt::new(StmtKind::Case { subject, case_kind: *case_kind, arms, default }, span)
            }
            UStmtKind::For { var, init, cond, step, body } => {
                self.unroll(var, init, cond, step, body, span)
            }
            UStmtKind::While { cond, body } => self.unroll_while(cond, body, span),
            UStmtKind::Call { name, args } => {
                // A task is called for what it writes back, so the statement is
                // whatever the body turned into.
                match self.inline_body(name, args, span) {
                    Ok(_) => Stmt::new(StmtKind::Block { stmts: Vec::new() }, span),
                    Err(message) => {
                        self.diags
                            .push(Diag::warning(DiagCode::UnsupportedConstruct, message).at(span));
                        Stmt::new(StmtKind::Unsupported { construct: name.clone() }, span)
                    }
                }
            }
            UStmtKind::Unsupported { construct } => {
                Stmt::new(StmtKind::Unsupported { construct: construct.clone() }, span)
            }
        }
    }

    /// Unrolls a `for` into the block of statements the hardware actually is.
    ///
    /// The loop variable is a constant inside each copy, so `data[i]` resolves
    /// to a different bit each time round — which is the whole point of writing
    /// the loop.
    fn unroll(
        &mut self,
        var: &str,
        init: &UExpr,
        cond: &UExpr,
        step: &UExpr,
        body: &UStmt,
        span: Span,
    ) -> Stmt {
        let Ok(mut value) = eval(init, self.scope) else {
            return self.unrollable(cond, span, "its starting value is not constant");
        };

        let mut stmts = Vec::new();
        let mut iterations = 0i64;
        // Set the first time the condition turns out to depend on a signal.
        let mut bound: Option<(i64, bool)> = None;

        loop {
            let mut scope = self.scope.clone();
            scope.bind(var, value);

            let guarded = match eval(cond, &scope) {
                Ok(0) => break,
                Ok(_) => false,
                // The condition is not known until the design runs, but the
                // number of times it *could* be true is: it is bounded by what
                // the signals in it can hold.
                Err(_) => {
                    let (limit, exact) = *bound.get_or_insert_with(|| self.guard_bound(cond));
                    if limit == 0 {
                        return self.unrollable(cond, span, "its condition is not constant");
                    }
                    if iterations >= limit {
                        if !exact {
                            self.capped(cond, span, limit);
                        }
                        break;
                    }
                    true
                }
            };

            iterations += 1;
            if iterations > MAX_UNROLL {
                return self.unrollable(cond, span, "it did not finish within the unrolling limit");
            }

            // The body is lowered against a scope where the loop variable is
            // this iteration's constant.
            let mut nested = ProcBuilder {
                scope: &scope,
                names: self.names,
                diags: self.diags,
                prelude: Vec::new(),
                depth: self.depth,
            };
            let mut lowered = nested.stmt(body);
            if guarded {
                // This copy only does anything on the iterations the loop
                // actually reaches — which is what makes the unrolled form the
                // same hardware as the loop.
                let cond = self.expr_in(cond, &scope);
                lowered = Stmt::new(
                    StmtKind::If { cond, then_branch: Box::new(lowered), else_branch: None },
                    span,
                );
            }
            stmts.push(lowered);

            let Ok(next) = eval(step, &scope) else {
                return self.unrollable(cond, span, "its step is not constant");
            };
            if next == value {
                return self.unrollable(cond, span, "it never advances");
            }
            value = next;
        }

        Stmt::new(StmtKind::Block { stmts }, span)
    }

    /// `while (cond) body`, as the copies of the body the hardware is.
    ///
    /// There is no counter to fold, so every copy is guarded. The number of
    /// them comes from the same place as for a `for` whose bound is a signal:
    /// what the signals in the condition can hold.
    fn unroll_while(&mut self, cond: &UExpr, body: &UStmt, span: Span) -> Stmt {
        // A condition that folds is not a loop at all: either it never runs, or
        // it never stops.
        if let Ok(value) = eval(cond, self.scope) {
            if value == 0 {
                return Stmt::new(StmtKind::Block { stmts: Vec::new() }, span);
            }
            return self.unrollable(cond, span, "its condition is always true");
        }

        let (limit, exact) = self.guard_bound(cond);
        if limit == 0 {
            return self.unrollable(cond, span, "nothing in its condition bounds how long it runs");
        }
        if !exact {
            self.capped(cond, span, limit);
        }

        let stmts = (0..limit)
            .map(|_| {
                let mut nested = ProcBuilder {
                    scope: self.scope,
                    names: self.names,
                    diags: self.diags,
                    prelude: Vec::new(),
                    depth: self.depth,
                };
                let body = nested.stmt(body);
                let cond = self.expr(cond);
                Stmt::new(
                    StmtKind::If { cond, then_branch: Box::new(body), else_branch: None },
                    span,
                )
            })
            .collect();
        Stmt::new(StmtKind::Block { stmts }, span)
    }

    /// How many times a condition that depends on signals could be true.
    ///
    /// The widest signal in it decides: `idx < amount` with `amount` three bits
    /// can be true at most eight times. Returns 0 when there is no signal to
    /// bound it by, which means the loop cannot be modelled at all, and whether
    /// the count is the real bound rather than the cap.
    fn guard_bound(&mut self, cond: &UExpr) -> (i64, bool) {
        let mut widest = 0u32;
        let mut found = false;
        collect_idents(cond, &mut |name| {
            if self.scope.get(name).is_some() {
                return;
            }
            if let Some(width) = self.names.declared_width_of(name) {
                widest = widest.max(width);
                found = true;
            }
        });
        if !found {
            return (0, false);
        }
        // 2^width, without letting a 32-bit bound ask for four billion copies.
        if widest >= 63 {
            return (MAX_GUARDED_UNROLL, false);
        }
        let exact = 1i64 << widest;
        (exact.min(MAX_GUARDED_UNROLL), exact <= MAX_GUARDED_UNROLL)
    }

    /// Lowers an expression against a scope other than this builder's.
    fn expr_in(&mut self, expr: &UExpr, scope: &Scope) -> Expr {
        let mut nested = ProcBuilder {
            scope,
            names: self.names,
            diags: self.diags,
            prelude: Vec::new(),
            depth: self.depth,
        };
        nested.expr(expr)
    }

    /// Says so when the number of copies was chosen by the cap rather than by
    /// the design.
    fn capped(&mut self, cond: &UExpr, span: Span, limit: i64) {
        self.diags.push(
            Diag::warning(
                DiagCode::UnsupportedConstruct,
                format!(
                    "`{cond}` is bounded by signals too wide to unroll exactly, so the loop \
                     is modelled as {limit} guarded copies; if it can run longer than that, \
                     the model is short"
                ),
            )
            .at(span),
        );
    }

    /// A loop that cannot be unrolled leaves a labelled hole, not a silence.
    fn unrollable(&mut self, cond: &UExpr, span: Span, why: &str) -> Stmt {
        self.diags.push(
            Diag::warning(
                DiagCode::UnsupportedConstruct,
                format!(
                    "`for` cannot be unrolled because {why} (`{cond}`); hardware has no                      loop to run, so the body is not modelled"
                ),
            )
            .at(span),
        );
        Stmt::new(StmtKind::Unsupported { construct: "for".to_string() }, span)
    }

    // ----------------------------------------------------------- expressions ---

    fn expr(&mut self, expr: &UExpr) -> Expr {
        let span = expr.span;
        let kind = match &*expr.kind {
            UExprKind::Ident { name } => {
                // A parameter shadows nothing — a name is either a constant or a
                // net, never both — so the constant scope is checked first.
                match self.scope.get(name) {
                    Some(value) => ExprKind::Lit { value: ConstBits::from_i64(32, value) },
                    None => {
                        let net = self.names.resolve_net(name, span, self.diags);
                        ExprKind::Ref { net: NetRef::Full { net } }
                    }
                }
            }
            UExprKind::Call { name, args } => return self.inline_call(name, args, span),
            UExprKind::Int { value } => ExprKind::Lit { value: ConstBits::from_i64(32, *value) },
            UExprKind::Sized { value } => ExprKind::Lit { value: value.clone() },
            UExprKind::Fill { ones } => {
                // Without an assignment context there is no width to fill, so
                // one bit stands in; width checking is a later pass.
                ExprKind::Lit { value: ConstBits::from_u64(1, u64::from(*ones)) }
            }
            UExprKind::Unary { op, operand } => {
                ExprKind::Unary { op: *op, operand: Box::new(self.expr(operand)) }
            }
            UExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op: *op,
                lhs: Box::new(self.expr(lhs)),
                rhs: Box::new(self.expr(rhs)),
            },
            UExprKind::Ternary { cond, then_value, else_value } => ExprKind::Ternary {
                cond: Box::new(self.expr(cond)),
                then_value: Box::new(self.expr(then_value)),
                else_value: Box::new(self.expr(else_value)),
            },
            UExprKind::Concat { parts } => {
                ExprKind::Concat { parts: parts.iter().map(|p| self.expr(p)).collect() }
            }
            UExprKind::Repl { count, value } => match eval(count, self.scope) {
                Ok(count) if count > 0 => {
                    ExprKind::Repl { count: count as u32, value: Box::new(self.expr(value)) }
                }
                _ => ExprKind::Unsupported { text: expr.to_string() },
            },
            UExprKind::Index { .. } | UExprKind::Range { .. } => {
                // A select resolves to a slice when its bounds are constant, and
                // to a dynamic index — a memory read — when they are not.
                return self.select_expr(expr);
            }
            UExprKind::Clog2 { .. } => match eval(expr, self.scope) {
                Ok(value) => ExprKind::Lit { value: ConstBits::from_i64(32, value) },
                Err(_) => ExprKind::Unsupported { text: expr.to_string() },
            },
            UExprKind::Cast { width, value } => {
                // A cast of a constant folds; a cast of a signal keeps the
                // value and drops the width, which the width pass will revisit.
                match (eval(width, self.scope), eval(expr, self.scope)) {
                    (Ok(width), Ok(folded)) if width > 0 => {
                        ExprKind::Lit { value: ConstBits::from_i64(width as u32, folded) }
                    }
                    _ => return self.expr(value),
                }
            }
            UExprKind::Unsupported { text } => ExprKind::Unsupported { text: text.clone() },
        };
        Expr::new(kind, span)
    }

    /// Copies a function body into the call site and yields the net holding
    /// its result.
    ///
    /// The copy is what synthesis does: `sat(a)` and `sat(b)` are two pieces of
    /// logic, not two uses of one. Each call gets its own nets — named
    /// `sat.0.a`, `sat.0` and so on — so that reads and writes stay derivable
    /// and the block diagram shows the logic rather than a call.
    fn inline_call(&mut self, name: &str, args: &[UExpr], span: Span) -> Expr {
        match self.inline_body(name, args, span) {
            Ok(Some(net)) => Expr::new(ExprKind::Ref { net: NetRef::Full { net } }, span),
            // A task has no result, so calling one where a value is wanted is a
            // mistake in the source rather than a gap here.
            Ok(None) => self.unsupported_expr(
                format!("`{name}` is a task and produces no value to use here"),
                format!("{name}(...)"),
                span,
            ),
            Err(message) => self.unsupported_expr(message, format!("{name}(...)"), span),
        }
    }

    /// Copies a function or task body into the call site.
    ///
    /// Returns the net holding a function's result, or `None` for a task, whose
    /// results reach the caller through its `output` arguments instead.
    fn inline_body(
        &mut self,
        name: &str,
        args: &[UExpr],
        _span: Span,
    ) -> Result<Option<rtlscope_ir::NetId>, String> {
        let Some(func) = self.names.function(name) else {
            return Err(format!("`{name}` is not a task or function declared in this module"));
        };
        if self.depth >= MAX_INLINE_DEPTH {
            return Err(format!(
                "`{name}` nests more than {MAX_INLINE_DEPTH} calls deep; a synthesisable \
                 function cannot recurse"
            ));
        }
        if func.args.len() != args.len() {
            return Err(format!(
                "`{name}` takes {} argument(s) but was called with {}",
                func.args.len(),
                args.len()
            ));
        }

        // The nets of the copy are named after the function alone: `double.0`
        // for a call of `defs::double`. The package is not part of what a
        // reader looks for in a waveform.
        let bare = name.rsplit("::").next().unwrap_or(name);
        let prefix = format!("{bare}.{}", self.names.next_call_site());
        let mut renames = Renames::new();
        // An argument that is constant at this call site makes the copy of the
        // body constant too — which is how a loop bounded by an argument
        // becomes unrollable, and is what synthesis does when it specialises a
        // function to its caller.
        let mut inner = self.scope.clone();
        // A function from a package names the package's constants bare, and
        // the caller need not have imported them: they are put in reach here,
        // over the caller's own names, as they are inside the package.
        if let Some(package) = &func.package
            && let Some(constants) = self.names.package_scope(package)
        {
            inner.absorb(&constants);
        }

        // SystemVerilog spells the return value as the function's own name, so
        // assignments to it inside the body become writes to this net. A task
        // has no such name to bind.
        let result = (!func.is_task).then(|| {
            let width =
                self.width_in(&inner, func.packed.as_ref(), func.net_type, func.type_name.as_ref());
            renames.insert(func.name.clone(), prefix.clone());
            self.names.declare_local(&prefix, width, func.span)
        });

        let mut copy_back = Vec::new();
        for (formal, actual) in func.args.iter().zip(args) {
            let local = format!("{prefix}.{}", formal.name);
            renames.insert(formal.name.clone(), local.clone());
            let width = self.width_in(
                &inner,
                formal.packed.as_ref(),
                formal.net_type,
                formal.type_name.as_ref(),
            );
            let net = self.names.declare_local(&local, width, formal.span);

            // An `output` argument is written by the body and read by nobody
            // here, so taking its value in would invent a read that the
            // hardware does not have.
            if formal.dir != PortDir::Output {
                if let Ok(value) = eval(actual, self.scope) {
                    inner.bind_sized(&local, value, width);
                }
                // Lowered before the assignment is pushed, so that a call inside
                // an argument lands ahead of the argument it feeds.
                let rhs = self.expr(actual);
                self.prelude.push(Stmt::new(
                    StmtKind::Assign {
                        lhs: NetRef::Full { net },
                        lhs_index: None,
                        rhs,
                        blocking: true,
                    },
                    formal.span,
                ));
            }

            if formal.dir != PortDir::Input {
                // What the body wrote goes back to the caller's own signal,
                // which is the only way a task returns anything.
                let (lhs, lhs_index) = self.assign_target(actual);
                copy_back.push(Stmt::new(
                    StmtKind::Assign {
                        lhs,
                        lhs_index,
                        rhs: Expr::new(ExprKind::Ref { net: NetRef::Full { net } }, formal.span),
                        blocking: true,
                    },
                    formal.span,
                ));
            }
        }

        for local in &func.locals {
            let renamed = format!("{prefix}.{}", local.name);
            renames.insert(local.name.clone(), renamed.clone());
            let width = self.width_in(
                &inner,
                local.packed.as_ref(),
                Some(local.net_type),
                local.type_name.as_ref(),
            );
            self.names.declare_local(&renamed, width, local.span);
            if local.unpacked.is_some() {
                self.diags.push(
                    Diag::warning(
                        DiagCode::UnsupportedConstruct,
                        format!(
                            "`{}` is an array local to `{name}`; it is modelled as a plain net, so indexing into it will not be right",
                            local.name
                        ),
                    )
                    .at(local.span),
                );
            }
        }

        let body = rename_stmt(&func.body, &renames);
        let mut nested = ProcBuilder {
            scope: &inner,
            names: self.names,
            diags: self.diags,
            prelude: Vec::new(),
            depth: self.depth + 1,
        };
        let body = nested.stmt(&body);
        self.prelude.push(body);
        self.prelude.extend(copy_back);

        Ok(result)
    }

    /// A hole in an expression, reported rather than guessed at.
    fn unsupported_expr(&mut self, message: String, text: String, span: Span) -> Expr {
        self.diags.push(Diag::warning(DiagCode::UnsupportedConstruct, message).at(span));
        Expr::new(ExprKind::Unsupported { text }, span)
    }

    /// How wide a function's argument, local or return value is.
    ///
    /// The same three sources as any other declaration: an explicit range, the
    /// width a keyword like `int` carries on its own, or a typedef.
    fn width_of(
        &self,
        packed: Option<&URange>,
        net_type: Option<UNetType>,
        type_name: Option<&String>,
    ) -> u32 {
        self.width_in(self.scope, packed, net_type, type_name)
    }

    /// The same, with the range folded in a given scope: the one a function
    /// body runs in, which for a package function holds the package's own
    /// constants.
    fn width_in(
        &self,
        scope: &Scope,
        packed: Option<&URange>,
        net_type: Option<UNetType>,
        type_name: Option<&String>,
    ) -> u32 {
        if let Some(range) = packed {
            return match (eval(&range.msb, scope), eval(&range.lsb, scope)) {
                (Ok(msb), Ok(lsb)) => (msb - lsb).unsigned_abs() as u32 + 1,
                _ => 1,
            };
        }
        if let Some(width) = net_type.and_then(UNetType::intrinsic_width) {
            return width;
        }
        type_name.and_then(|name| self.names.type_width(name)).unwrap_or(1)
    }

    fn select_expr(&mut self, expr: &UExpr) -> Expr {
        let span = expr.span;
        // A select out of a parameter is arithmetic on a constant, not a slice
        // of a net: `LINE_PIXELS[15:0]` names no signal at all. Reaching for a
        // net first invented one, one bit wide, for every such expression.
        if selects_a_constant(expr, self.scope)
            && let Ok(value) = eval(expr, self.scope)
        {
            let width = select_width(expr, self.scope).unwrap_or(32);
            return Expr::new(ExprKind::Lit { value: ConstBits::from_i64(width, value) }, span);
        }
        match &*expr.kind {
            UExprKind::Index { base, index } => {
                let Some(name) = ident_name(base) else {
                    return Expr::new(ExprKind::Unsupported { text: expr.to_string() }, span);
                };
                let net = self.names.resolve_net(&name, span, self.diags);
                match eval(index, self.scope) {
                    Ok(bit) if bit >= 0 => Expr::new(
                        ExprKind::Ref {
                            net: NetRef::Slice { net, msb: bit as u32, lsb: bit as u32 },
                        },
                        span,
                    ),
                    // `mem[addr]` — the index is a signal, so this is a read
                    // port, not a slice.
                    _ => {
                        let index = Box::new(self.expr(index));
                        Expr::new(ExprKind::DynIndex { net, index }, span)
                    }
                }
            }
            UExprKind::Range { base, msb, lsb } => {
                let Some(name) = ident_name(base) else {
                    return Expr::new(ExprKind::Unsupported { text: expr.to_string() }, span);
                };
                let net = self.names.resolve_net(&name, span, self.diags);
                match (eval(msb, self.scope), eval(lsb, self.scope)) {
                    (Ok(msb), Ok(lsb)) if msb >= 0 && lsb >= 0 => Expr::new(
                        ExprKind::Ref {
                            net: NetRef::Slice {
                                net,
                                msb: msb.max(lsb) as u32,
                                lsb: msb.min(lsb) as u32,
                            },
                        },
                        span,
                    ),
                    _ => Expr::new(ExprKind::Unsupported { text: expr.to_string() }, span),
                }
            }
            _ => Expr::new(ExprKind::Unsupported { text: expr.to_string() }, span),
        }
    }

    /// An assignment target: the net written, plus the address if it is a
    /// memory write port.
    fn assign_target(&mut self, expr: &UExpr) -> (NetRef, Option<Box<Expr>>) {
        if let UExprKind::Index { .. } = &*expr.kind
            && let ExprKind::DynIndex { net, index } = self.select_expr(expr).kind
        {
            // `mem[addr] <= x` writes the whole memory and reads `addr`.
            return (NetRef::Full { net }, Some(index));
        }
        (self.net_ref(expr), None)
    }

    /// A bare net reference — a clock, a reset, or a simple assignment target.
    fn net_ref(&mut self, expr: &UExpr) -> NetRef {
        match &*expr.kind {
            UExprKind::Ident { name } => {
                NetRef::Full { net: self.names.resolve_net(name, expr.span, self.diags) }
            }
            UExprKind::Index { .. } | UExprKind::Range { .. } => {
                match self.select_expr(expr).kind {
                    ExprKind::Ref { net } => net,
                    // `mem[addr] <= x` writes the whole memory as far as the
                    // dataflow graph is concerned.
                    ExprKind::DynIndex { net, .. } => NetRef::Full { net },
                    _ => self.unresolvable_target(expr),
                }
            }
            _ => self.unresolvable_target(expr),
        }
    }

    fn unresolvable_target(&mut self, expr: &UExpr) -> NetRef {
        self.diags.push(
            Diag::warning(
                DiagCode::UnsupportedExpr,
                format!("`{expr}` is not a form RTLScope can treat as a signal here"),
            )
            .at(expr.span),
        );
        // A one-bit constant stands in so the statement keeps its shape; the
        // diagnostic is what tells the user the driver is not modelled.
        NetRef::Const { value: ConstBits::zero(1) }
    }
}

/// The signal an `if` at the very top of a process tests, and the level it is
/// active at. `if (!rst_n)` gives `(rst_n, Low)`; `if (rst)` gives `(rst, High)`.
fn leading_guard(body: &Stmt) -> Option<(NetRef, Level)> {
    let mut current = body;
    // Walk through a single-statement `begin ... end`, which is how almost
    // every reset is actually written.
    loop {
        match &current.kind {
            StmtKind::Block { stmts } if stmts.len() == 1 => current = &stmts[0],
            _ => break,
        }
    }

    let StmtKind::If { cond, .. } = &current.kind else { return None };
    match &cond.kind {
        ExprKind::Ref { net } => Some((net.clone(), Level::High)),
        ExprKind::Unary { op: rtlscope_ir::UnOp::LogNot, operand } => match &operand.kind {
            ExprKind::Ref { net } => Some((net.clone(), Level::Low)),
            _ => None,
        },
        _ => None,
    }
}

/// True when the `then` branch of the leading `if` assigns only constants,
/// which is what a reset does and what an enable does not.
fn leading_branch_is_constant(body: &Stmt) -> bool {
    let mut current = body;
    loop {
        match &current.kind {
            StmtKind::Block { stmts } if stmts.len() == 1 => current = &stmts[0],
            _ => break,
        }
    }
    let StmtKind::If { then_branch, .. } = &current.kind else { return false };

    let mut assignments = 0usize;
    let mut all_constant = true;
    then_branch.for_each_stmt(&mut |stmt| {
        if let StmtKind::Assign { lhs_index, rhs, .. } = &stmt.kind {
            assignments += 1;
            if lhs_index.is_some() {
                all_constant = false;
            }
            rhs.for_each_ref(&mut |net_ref| {
                if !net_ref.is_const() {
                    all_constant = false;
                }
            });
        }
    });
    assignments > 0 && all_constant
}

/// Invariant 2: `reads` and `writes` are exactly what a traversal of the body
/// yields, plus the clock and reset a flop is sensitive to.
pub(crate) fn derive_reads_writes(body: &Stmt, kind: &ProcKind) -> (Vec<NetRef>, Vec<NetRef>) {
    let mut reads = Vec::new();
    let mut writes = Vec::new();

    // The clock and reset are read by the process even though the body never
    // names them, and the register-adjacency graph needs to see that.
    if let ProcKind::Ff { clk, rst, .. } = kind {
        reads.push(clk.clone());
        if let Some(reset) = rst {
            reads.push(reset.net.clone());
        }
    }

    collect(body, &mut reads, &mut writes);

    dedupe(&mut reads);
    dedupe(&mut writes);
    (reads, writes)
}

fn collect(stmt: &Stmt, reads: &mut Vec<NetRef>, writes: &mut Vec<NetRef>) {
    match &stmt.kind {
        StmtKind::Block { stmts } => {
            for stmt in stmts {
                collect(stmt, reads, writes);
            }
        }
        StmtKind::Assign { lhs, lhs_index, rhs, .. } => {
            if !lhs.is_const() {
                writes.push(lhs.clone());
            }
            if let Some(index) = lhs_index {
                index.for_each_ref(&mut |r| push_read(reads, r));
            }
            rhs.for_each_ref(&mut |r| push_read(reads, r));
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            cond.for_each_ref(&mut |r| push_read(reads, r));
            collect(then_branch, reads, writes);
            if let Some(else_branch) = else_branch {
                collect(else_branch, reads, writes);
            }
        }
        StmtKind::Case { subject, arms, default, .. } => {
            subject.for_each_ref(&mut |r| push_read(reads, r));
            for arm in arms {
                for label in &arm.labels {
                    label.for_each_ref(&mut |r| push_read(reads, r));
                }
                collect(&arm.body, reads, writes);
            }
            if let Some(default) = default {
                collect(default, reads, writes);
            }
        }
        StmtKind::Unsupported { .. } => {}
    }
}

fn push_read(reads: &mut Vec<NetRef>, net_ref: &NetRef) {
    if !net_ref.is_const() {
        reads.push(net_ref.clone());
    }
}

/// Keeps one entry per net, so the dataflow graph gets one edge per dependency
/// rather than one per mention.
///
/// Where a net is touched through more than one slice — `a[0]` through `a[7]`,
/// as an unrolled loop produces — the entry becomes the whole net. Keeping the
/// first slice would say the process reads one bit when it reads all of them.
fn dedupe(refs: &mut Vec<NetRef>) {
    let mut counts: std::collections::HashMap<rtlscope_ir::NetId, usize> =
        std::collections::HashMap::new();
    for net_ref in refs.iter() {
        if let Some(net) = net_ref.net_id() {
            *counts.entry(net).or_default() += 1;
        }
    }

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(refs.len());
    for net_ref in refs.drain(..) {
        let Some(net) = net_ref.net_id() else { continue };
        if !seen.insert(net) {
            continue;
        }
        let whole = counts.get(&net).copied().unwrap_or(0) > 1;
        out.push(if whole { NetRef::Full { net } } else { net_ref });
    }
    *refs = out;
}

fn ident_name(expr: &UExpr) -> Option<String> {
    match &*expr.kind {
        UExprKind::Ident { name } => Some(name.clone()),
        _ => None,
    }
}

/// Whether the thing being selected out of is a constant rather than a signal.
fn selects_a_constant(expr: &UExpr, scope: &Scope) -> bool {
    match &*expr.kind {
        UExprKind::Index { base, .. } | UExprKind::Range { base, .. } => {
            selects_a_constant(base, scope)
        }
        UExprKind::Ident { name } => scope.get(name).is_some(),
        _ => false,
    }
}

/// How many bits a select yields, when its bounds are constant.
fn select_width(expr: &UExpr, scope: &Scope) -> Option<u32> {
    match &*expr.kind {
        UExprKind::Index { .. } => Some(1),
        UExprKind::Range { msb, lsb, .. } => {
            let msb = eval(msb, scope).ok()?;
            let lsb = eval(lsb, scope).ok()?;
            Some((msb - lsb).unsigned_abs() as u32 + 1)
        }
        _ => None,
    }
}

/// Visits every identifier in an expression.
fn collect_idents(expr: &UExpr, visit: &mut impl FnMut(&str)) {
    match &*expr.kind {
        UExprKind::Ident { name } => visit(name),
        UExprKind::Int { .. } | UExprKind::Sized { .. } | UExprKind::Fill { .. } => {}
        UExprKind::Unsupported { .. } => {}
        UExprKind::Unary { operand, .. } => collect_idents(operand, visit),
        UExprKind::Binary { lhs, rhs, .. } => {
            collect_idents(lhs, visit);
            collect_idents(rhs, visit);
        }
        UExprKind::Ternary { cond, then_value, else_value } => {
            collect_idents(cond, visit);
            collect_idents(then_value, visit);
            collect_idents(else_value, visit);
        }
        UExprKind::Concat { parts } => parts.iter().for_each(|p| collect_idents(p, visit)),
        UExprKind::Repl { count, value } => {
            collect_idents(count, visit);
            collect_idents(value, visit);
        }
        UExprKind::Index { base, index } => {
            collect_idents(base, visit);
            collect_idents(index, visit);
        }
        UExprKind::Range { base, msb, lsb } => {
            collect_idents(base, visit);
            collect_idents(msb, visit);
            collect_idents(lsb, visit);
        }
        UExprKind::Clog2 { arg } => collect_idents(arg, visit),
        UExprKind::Cast { width, value } => {
            collect_idents(width, visit);
            collect_idents(value, visit);
        }
        UExprKind::Call { args, .. } => args.iter().for_each(|a| collect_idents(a, visit)),
    }
}
