//! Procedural bodies and continuous assignments.
//!
//! `always_ff @(posedge clk) begin ... end` puts its sensitivity list *inside*
//! the statement, as a `ProceduralTimingControlStatement` wrapping the real
//! body. The unresolved IR keeps the events on the process instead, because
//! that is what elaboration classifies into flop-or-combinational, so the
//! wrapper is unwrapped here and never appears downstream.
//!
//! Only what the synthesisable subset allows is translated: blocking and
//! non-blocking assignment, `if`, `case`/`casez`, and `begin`/`end`. Loops,
//! `fork`, task calls and `casex` become an `Unsupported` statement carrying
//! the keyword the author wrote — the body keeps its shape, with a labelled
//! hole in it, rather than being thrown away whole.

use rtlscope_ir::{
    BinOp, CaseKind, Edge, Span, UCaseArm, UEvent, UExpr, UExprKind, UItem, UNet, UProcKind, UStmt,
    UStmtKind, UnOp,
};
use sv_parser::{self as sv, RefNode, SyntaxTree, unwrap_node};

use crate::lower::Lowerer;
use crate::nav;

impl Lowerer {
    // --------------------------------------------------- continuous assign ---

    /// `assign a = b, c = d;` — one [`UItem::Assign`] per assignment.
    pub(crate) fn continuous_assign(&mut self, tree: &SyntaxTree, node: RefNode<'_>) -> Vec<UItem> {
        let mut out = Vec::new();
        for child in node.clone().into_iter() {
            let (lvalue, expression, whole) = match &child {
                RefNode::NetAssignment(x) => {
                    (RefNode::NetLvalue(&x.nodes.0), &x.nodes.2, RefNode::NetAssignment(x))
                }
                RefNode::VariableAssignment(x) => (
                    RefNode::VariableLvalue(&x.nodes.0),
                    &x.nodes.2,
                    RefNode::VariableAssignment(x),
                ),
                _ => continue,
            };
            let span = self.span(tree, whole);
            let lhs = self.lvalue(tree, lvalue);
            let rhs = self.expr(tree).expression(expression);
            out.push(UItem::Assign { lhs, rhs, span });
        }

        if out.is_empty() {
            return vec![self.unsupported_item(tree, node)];
        }
        out
    }

    // ------------------------------------------------------------ processes ---

    /// `initial begin ... end` — what the design starts out holding.
    pub(crate) fn initial_construct(
        &mut self,
        tree: &SyntaxTree,
        construct: &sv::InitialConstruct,
    ) -> UItem {
        let (_keyword, statement) = &construct.nodes;
        let span = self.span(tree, RefNode::InitialConstruct(construct));
        let body = self.statement_or_null(tree, statement);
        UItem::Proc { kind: UProcKind::Initial, body, span }
    }

    pub(crate) fn always_construct(
        &mut self,
        tree: &SyntaxTree,
        construct: &sv::AlwaysConstruct,
    ) -> UItem {
        let (keyword, statement) = &construct.nodes;
        let span = self.span(tree, RefNode::AlwaysConstruct(construct));

        // The sensitivity list is the outermost statement, not part of the
        // header, so peel it off before lowering the body.
        let (events, star, body_statement) = self.peel_event_control(tree, statement);

        let kind = match keyword {
            sv::AlwaysKeyword::AlwaysComb(_) => UProcKind::AlwaysComb,
            sv::AlwaysKeyword::AlwaysLatch(_) => UProcKind::AlwaysLatch,
            sv::AlwaysKeyword::AlwaysFf(_) => UProcKind::AlwaysFf { events },
            sv::AlwaysKeyword::Always(_) => UProcKind::AlwaysAt { events, star },
        };

        let body = match body_statement {
            Some(statement) => self.statement_or_null(tree, statement),
            None => self.statement(tree, statement),
        };
        UItem::Proc { kind, body, span }
    }

    /// Splits `@(posedge clk or negedge rst_n) <body>` into its event list and
    /// its body. Returns no events and no inner statement when the process has
    /// no timing control, as `always_comb` does.
    fn peel_event_control<'t>(
        &mut self,
        tree: &SyntaxTree,
        statement: &'t sv::Statement,
    ) -> (Vec<UEvent>, bool, Option<&'t sv::StatementOrNull>) {
        let (_label, _attrs, item) = &statement.nodes;
        let sv::StatementItem::ProceduralTimingControlStatement(timed) = item else {
            return (Vec::new(), false, None);
        };
        let (control, body) = &timed.nodes;
        let sv::ProceduralTimingControl::EventControl(control) = control else {
            // `#10 <body>` — a delay. Simulation-only, so the events are empty
            // and the body is still lowered.
            return (Vec::new(), false, Some(body));
        };

        let mut events = Vec::new();
        let mut star = false;
        match &**control {
            sv::EventControl::EventExpression(expression) => {
                self.event_expression(tree, &expression.nodes.1.nodes.1, &mut events);
            }
            // `always @*` and `always @(*)`.
            sv::EventControl::Asterisk(_) | sv::EventControl::ParenAsterisk(_) => star = true,
            other => {
                let span = self.span(tree, RefNode::EventControl(other));
                events.push(UEvent {
                    edge: None,
                    expr: UExpr::new(
                        rtlscope_ir::UExprKind::Unsupported {
                            text: nav::subtree_text(tree, RefNode::EventControl(other)),
                        },
                        span,
                    ),
                });
            }
        }
        (events, star, Some(body))
    }

    /// Flattens `a or b, posedge c` into a list.
    fn event_expression(
        &mut self,
        tree: &SyntaxTree,
        expression: &sv::EventExpression,
        out: &mut Vec<UEvent>,
    ) {
        match expression {
            sv::EventExpression::Expression(e) => {
                let (edge, expression, _iff) = &e.nodes;
                let edge = edge.as_ref().and_then(|edge| {
                    match self.keyword_of(tree, RefNode::EdgeIdentifier(edge)).as_str() {
                        "posedge" => Some(Edge::Pos),
                        "negedge" => Some(Edge::Neg),
                        // `edge` means either; treated as level-sensitive so it
                        // is never mistaken for a clock.
                        _ => None,
                    }
                });
                let expr = self.expr(tree).expression(expression);
                out.push(UEvent { edge, expr });
            }
            sv::EventExpression::Or(or) => {
                self.event_expression(tree, &or.nodes.0, out);
                self.event_expression(tree, &or.nodes.2, out);
            }
            sv::EventExpression::Comma(comma) => {
                self.event_expression(tree, &comma.nodes.0, out);
                self.event_expression(tree, &comma.nodes.2, out);
            }
            sv::EventExpression::Paren(paren) => {
                self.event_expression(tree, &paren.nodes.0.nodes.1, out)
            }
            sv::EventExpression::Sequence(sequence) => {
                let span = self.span(tree, RefNode::EventExpressionSequence(sequence));
                out.push(UEvent {
                    edge: None,
                    expr: UExpr::new(
                        rtlscope_ir::UExprKind::Unsupported {
                            text: nav::subtree_text(
                                tree,
                                RefNode::EventExpressionSequence(sequence),
                            ),
                        },
                        span,
                    ),
                });
            }
        }
    }

    // ----------------------------------------------------------- statements ---

    /// A statement in a function body.
    ///
    /// The grammar gives functions their own statement type only to forbid
    /// timing controls inside them; what is left lowers exactly as a procedural
    /// statement does.
    pub(crate) fn function_statement(
        &mut self,
        tree: &SyntaxTree,
        statement: &sv::FunctionStatementOrNull,
    ) -> UStmt {
        match statement {
            sv::FunctionStatementOrNull::Statement(inner) => self.statement(tree, &inner.nodes.0),
            sv::FunctionStatementOrNull::Attribute(attribute) => {
                let span = self.span(tree, RefNode::FunctionStatementOrNullAttribute(attribute));
                UStmt::new(UStmtKind::Block { stmts: Vec::new(), decls: Vec::new() }, span)
            }
        }
    }

    pub(crate) fn statement_or_null(
        &mut self,
        tree: &SyntaxTree,
        statement: &sv::StatementOrNull,
    ) -> UStmt {
        match statement {
            sv::StatementOrNull::Statement(statement) => self.statement(tree, statement),
            // `;` on its own — an empty statement, which is legal and does
            // nothing, so it lowers to an empty block rather than a gap.
            sv::StatementOrNull::Attribute(attribute) => {
                let span = self.span(tree, RefNode::StatementOrNullAttribute(attribute));
                UStmt::new(UStmtKind::Block { stmts: Vec::new(), decls: Vec::new() }, span)
            }
        }
    }

    fn statement(&mut self, tree: &SyntaxTree, statement: &sv::Statement) -> UStmt {
        let (_label, _attrs, item) = &statement.nodes;
        let span = self.span(tree, RefNode::Statement(statement));

        match item {
            sv::StatementItem::SeqBlock(block) => {
                let (_begin, _label, declarations, statements, _end, _tail) = &block.nodes;
                // Inside a function these were already hoisted to the function
                // itself, where the inliner can give each call site its own
                // copy; declaring them twice would shadow that.
                let decls = if self.fn_name.is_some() {
                    Vec::new()
                } else {
                    self.block_declarations(tree, declarations)
                };
                let returns: Vec<bool> = statements
                    .iter()
                    .map(|s| unwrap_node!(RefNode::StatementOrNull(s), JumpStatement).is_some())
                    .collect();
                let lowered = statements.iter().map(|s| self.statement_or_null(tree, s)).collect();
                let stmts = self.guard_after_return(lowered, &returns, span);
                UStmt::new(UStmtKind::Block { stmts, decls }, span)
            }

            sv::StatementItem::BlockingAssignment(assignment) => {
                self.blocking_assignment(tree, &assignment.0, span)
            }

            sv::StatementItem::NonblockingAssignment(assignment) => {
                let (lvalue, _arrow, delay, expression) = &assignment.0.nodes;
                if delay.is_some() {
                    // `q <= #1 d` — a simulation delay that synthesis ignores.
                    // Modelling it as a plain assignment would be a lie about
                    // what the hardware does.
                    return self
                        .unsupported_stmt(tree, RefNode::NonblockingAssignment(&assignment.0));
                }
                let lhs = self.lvalue(tree, RefNode::VariableLvalue(lvalue));
                let rhs = self.expr(tree).expression(expression);
                UStmt::new(UStmtKind::Assign { lhs, rhs, blocking: false }, span)
            }

            sv::StatementItem::ConditionalStatement(conditional) => {
                self.conditional(tree, conditional, span)
            }

            sv::StatementItem::CaseStatement(case) => self.case(tree, case, span),

            sv::StatementItem::JumpStatement(jump) => self.jump(tree, jump, span),

            sv::StatementItem::IncOrDecExpression(expression) => {
                self.inc_or_dec(tree, &expression.0, span)
            }

            sv::StatementItem::SubroutineCallStatement(call) => {
                self.subroutine_call_statement(tree, call, span)
            }

            sv::StatementItem::LoopStatement(loop_statement) => {
                self.loop_statement(tree, loop_statement, span)
            }

            // A timing control nested inside a body, e.g. `@(posedge clk) x <= y`
            // in the middle of a block. Outside the subset.
            other => self.unsupported_stmt(tree, RefNode::StatementItem(other)),
        }
    }

    fn blocking_assignment(
        &mut self,
        tree: &SyntaxTree,
        assignment: &sv::BlockingAssignment,
        span: Span,
    ) -> UStmt {
        match assignment {
            sv::BlockingAssignment::OperatorAssignment(operator) => {
                let (lvalue, op, expression) = &operator.nodes;
                let operator_text = self.keyword_of(tree, RefNode::AssignmentOperator(op));
                let lhs = self.lvalue(tree, RefNode::VariableLvalue(lvalue));
                let value = self.expr(tree).expression(expression);
                // `x += e` is `x = x + e`. Folding the operator into the
                // right-hand side keeps one kind of assignment in the IR; the
                // left-hand side reads as well as it writes, selects included.
                let rhs = match operator_text.as_str() {
                    "=" => value,
                    other => {
                        let Some(op) = compound_operator(other) else {
                            return self
                                .unsupported_stmt(tree, RefNode::OperatorAssignment(operator));
                        };
                        UExpr::new(UExprKind::Binary { op, lhs: lhs.clone(), rhs: value }, span)
                    }
                };
                UStmt::new(UStmtKind::Assign { lhs, rhs, blocking: true }, span)
            }
            other => self.unsupported_stmt(tree, RefNode::BlockingAssignment(other)),
        }
    }

    fn conditional(
        &mut self,
        tree: &SyntaxTree,
        conditional: &sv::ConditionalStatement,
        span: Span,
    ) -> UStmt {
        let (_unique, _keyword, predicate, then_branch, else_ifs, else_branch) = &conditional.nodes;

        // `else if` chains are built from the tail backwards, so the nesting
        // comes out the same shape the author wrote.
        let mut tail = else_branch
            .as_ref()
            .map(|(_keyword, statement)| self.statement_or_null(tree, statement));

        for (_else_keyword, _if_keyword, predicate, statement) in else_ifs.iter().rev() {
            let cond = self.cond_predicate(tree, &predicate.nodes.1);
            let then_branch = self.statement_or_null(tree, statement);
            let span = then_branch.span;
            tail = Some(UStmt::new(UStmtKind::If { cond, then_branch, else_branch: tail }, span));
        }

        let cond = self.cond_predicate(tree, &predicate.nodes.1);
        let then_branch = self.statement_or_null(tree, then_branch);
        UStmt::new(UStmtKind::If { cond, then_branch, else_branch: tail }, span)
    }

    /// The variables a `begin ... end` declares for itself.
    fn block_declarations(
        &mut self,
        tree: &SyntaxTree,
        declarations: &[sv::BlockItemDeclaration],
    ) -> Vec<UNet> {
        let mut out = Vec::new();
        for declaration in declarations {
            let node = RefNode::BlockItemDeclaration(declaration);
            for item in self.net_declaration(tree, node) {
                if let UItem::Net { net } = item {
                    out.push(net);
                }
            }
        }
        out
    }

    /// `load_tx_byte(op, idx, addr, value, byte);` — a task called for what it
    /// writes back rather than for a value.
    ///
    /// A system call in the same position (`$display`, `$fatal`) describes
    /// simulation, not hardware, and is reported instead.
    fn subroutine_call_statement(
        &mut self,
        tree: &SyntaxTree,
        call: &sv::SubroutineCallStatement,
        span: Span,
    ) -> UStmt {
        let whole = RefNode::SubroutineCallStatement(call);
        let sv::SubroutineCallStatement::SubroutineCall(inner) = call else {
            return self.unsupported_stmt(tree, whole);
        };
        let sv::SubroutineCall::TfCall(tf) = &inner.0 else {
            return self.unsupported_stmt(tree, whole);
        };
        let (identifier, _attrs, args) = &tf.nodes;
        let Some(name) = nav::name_str(tree, RefNode::PsOrHierarchicalTfIdentifier(identifier))
        else {
            return self.unsupported_stmt(tree, whole);
        };
        // `finish_up;` with no parentheses is still a call.
        let args = match args {
            Some(paren) => match self.expr(tree).ordered_arguments(&paren.nodes.1) {
                Some(args) => args,
                None => return self.unsupported_stmt(tree, whole),
            },
            None => Vec::new(),
        };
        UStmt::new(UStmtKind::Call { name, args }, span)
    }

    /// `count++;` as a statement of its own, which is `count = count + 1`.
    fn inc_or_dec(
        &mut self,
        tree: &SyntaxTree,
        expression: &sv::IncOrDecExpression,
        span: Span,
    ) -> UStmt {
        let (lvalue, operator) = match expression {
            sv::IncOrDecExpression::Prefix(prefix) => (&prefix.nodes.2, &prefix.nodes.0),
            sv::IncOrDecExpression::Suffix(suffix) => (&suffix.nodes.0, &suffix.nodes.2),
        };
        let op = match self.keyword_of(tree, RefNode::IncOrDecOperator(operator)).as_str() {
            "++" => BinOp::Add,
            "--" => BinOp::Sub,
            _ => return self.unsupported_stmt(tree, RefNode::IncOrDecExpression(expression)),
        };
        let lhs = self.lvalue(tree, RefNode::VariableLvalue(lvalue));
        let rhs =
            UExpr::new(UExprKind::Binary { op, lhs: lhs.clone(), rhs: UExpr::int(1, span) }, span);
        UStmt::new(UStmtKind::Assign { lhs, rhs, blocking: true }, span)
    }

    /// Everything after a statement that may return runs only if it did not.
    ///
    /// One guard around the whole tail, rather than one per statement: it says
    /// the same thing and reads as what it is.
    pub(crate) fn guard_after_return(
        &mut self,
        mut stmts: Vec<UStmt>,
        returns: &[bool],
        span: Span,
    ) -> Vec<UStmt> {
        let Some(flag) = self.fn_flag.clone() else { return stmts };
        let Some(first) = returns.iter().position(|r| *r) else { return stmts };
        if first + 1 >= stmts.len() {
            return stmts;
        }

        let tail: Vec<UStmt> = stmts.split_off(first + 1);
        let cond = UExpr::new(
            UExprKind::Unary { op: UnOp::LogNot, operand: UExpr::ident(&flag, span) },
            span,
        );
        stmts.push(UStmt::new(
            UStmtKind::If {
                cond,
                then_branch: UStmt::new(UStmtKind::Block { stmts: tail, decls: Vec::new() }, span),
                else_branch: None,
            },
            span,
        ));
        stmts
    }

    /// `return expr;` inside a function.
    ///
    /// The result of a function is a variable named after the function, so a
    /// `return` is an assignment to it — plus, when the function can leave
    /// early, a note that it has, so the statements after it stand down.
    /// `break` and `continue` change control flow rather than data and have no
    /// equivalent here.
    fn jump(&mut self, tree: &SyntaxTree, jump: &sv::JumpStatement, span: Span) -> UStmt {
        let sv::JumpStatement::Return(ret) = jump else {
            return self.unsupported_stmt(tree, RefNode::JumpStatement(jump));
        };
        let (_keyword, expression, _semi) = &ret.nodes;
        let (Some(name), Some(expression)) = (self.fn_name.clone(), expression.as_ref()) else {
            return self.unsupported_stmt(tree, RefNode::JumpStatementReturn(ret));
        };
        let rhs = self.expr(tree).expression(expression);
        let assign = UStmt::new(
            UStmtKind::Assign { lhs: UExpr::ident(&name, span), rhs, blocking: true },
            span,
        );
        let Some(flag) = self.fn_flag.clone() else { return assign };
        let raise = UStmt::new(
            UStmtKind::Assign {
                lhs: UExpr::ident(&flag, span),
                rhs: UExpr::int(1, span),
                blocking: true,
            },
            span,
        );
        UStmt::new(UStmtKind::Block { stmts: vec![assign, raise], decls: Vec::new() }, span)
    }

    /// `for (int i = 0; i < N; i++)` and `while (cond)`.
    ///
    /// `forever`, `repeat` and `do`/`while` describe a number of iterations
    /// with nothing to bound it by; `foreach` would need array bounds the
    /// subset does not track.
    fn loop_statement(
        &mut self,
        tree: &SyntaxTree,
        loop_statement: &sv::LoopStatement,
        span: Span,
    ) -> UStmt {
        let for_loop = match loop_statement {
            sv::LoopStatement::For(for_loop) => for_loop,
            // `while` has no counter, but it still describes a bounded number
            // of copies of its body once the condition's operands are known.
            sv::LoopStatement::While(while_loop) => {
                let (_keyword, paren, body) = &while_loop.nodes;
                let cond = self.expr(tree).expression(&paren.nodes.1);
                let body = Box::new(self.statement_or_null(tree, body));
                return UStmt::new(UStmtKind::While { cond, body }, span);
            }
            _ => return self.unsupported_stmt(tree, RefNode::LoopStatement(loop_statement)),
        };
        let (_keyword, paren, body) = &for_loop.nodes;
        let (initialisation, _semi, condition, _semi2, step) = &paren.nodes.1;

        let Some((var, init)) = initialisation
            .as_ref()
            .and_then(|initialisation| self.for_initialisation(tree, initialisation))
        else {
            return self.unsupported_stmt(tree, RefNode::LoopStatementFor(for_loop));
        };
        let Some(condition) = condition else {
            // `for (i = 0; ; i++)` never ends, so there is nothing to unroll.
            return self.unsupported_stmt(tree, RefNode::LoopStatementFor(for_loop));
        };
        let cond = self.expr(tree).expression(condition);

        let Some(step) = step.as_ref().and_then(|step| self.for_step(tree, step, &var)) else {
            return self.unsupported_stmt(tree, RefNode::LoopStatementFor(for_loop));
        };

        let body = self.statement_or_null(tree, body);
        UStmt::new(UStmtKind::For { var, init, cond, step, body }, span)
    }

    /// The loop variable and where it starts, from either `int i = 0` or a
    /// plain `i = 0` over a variable declared outside.
    fn for_initialisation(
        &mut self,
        tree: &SyntaxTree,
        initialisation: &sv::ForInitialization,
    ) -> Option<(String, UExpr)> {
        match initialisation {
            sv::ForInitialization::Declaration(declaration) => {
                // One variable only: `for (int i = 0, j = 0; ...)` would unroll
                // over two counters at once, which the subset does not model.
                let variables = declaration.nodes.0.contents();
                let [variable] = variables.as_slice() else { return None };
                let assignments = variable.nodes.2.contents();
                match assignments.as_slice() {
                    [(identifier, _eq, expression)] => {
                        let name =
                            nav::identifier_str(tree, RefNode::VariableIdentifier(identifier))?;
                        Some((name, self.expr(tree).expression(expression)))
                    }
                    _ => None,
                }
            }
            sv::ForInitialization::ListOfVariableAssignments(assignments) => {
                match assignments.nodes.0.contents().as_slice() {
                    [assignment] => {
                        let (lvalue, _eq, expression) = &assignment.nodes;
                        let name = nav::identifier_str(tree, RefNode::VariableLvalue(lvalue))?;
                        Some((name, self.expr(tree).expression(expression)))
                    }
                    _ => None,
                }
            }
        }
    }

    /// The value the loop variable takes next: the right-hand side of `i = i+1`,
    /// or `i + 1` desugared from `i++`.
    fn for_step(&mut self, tree: &SyntaxTree, step: &sv::ForStep, var: &str) -> Option<UExpr> {
        let assignments = step.nodes.0.contents();
        let [assignment] = assignments.as_slice() else { return None };
        match assignment {
            sv::ForStepAssignment::OperatorAssignment(operator) => {
                let (lvalue, op, expression) = &operator.nodes;
                let operator_text = self.keyword_of(tree, RefNode::AssignmentOperator(op));
                let target = nav::identifier_str(tree, RefNode::VariableLvalue(lvalue))?;
                if target != var {
                    return None;
                }
                let value = self.expr(tree).expression(expression);
                match operator_text.as_str() {
                    "=" => Some(value),
                    // `i += 2` is `i = i + 2`, and so on for the rest.
                    other => {
                        let op = compound_operator(other)?;
                        let span = value.span;
                        Some(UExpr::new(
                            UExprKind::Binary { op, lhs: UExpr::ident(var, span), rhs: value },
                            span,
                        ))
                    }
                }
            }
            sv::ForStepAssignment::IncOrDecExpression(expression) => {
                let (operator, whole) = match &**expression {
                    sv::IncOrDecExpression::Prefix(prefix) => {
                        (&prefix.nodes.0, RefNode::IncOrDecExpressionPrefix(prefix))
                    }
                    sv::IncOrDecExpression::Suffix(suffix) => {
                        (&suffix.nodes.2, RefNode::IncOrDecExpressionSuffix(suffix))
                    }
                };
                let op = match self.keyword_of(tree, RefNode::IncOrDecOperator(operator)).as_str() {
                    "++" => BinOp::Add,
                    "--" => BinOp::Sub,
                    _ => return None,
                };
                let span = self.span(tree, whole);
                Some(UExpr::new(
                    UExprKind::Binary {
                        op,
                        lhs: UExpr::ident(var, span),
                        rhs: UExpr::int(1, span),
                    },
                    span,
                ))
            }
            sv::ForStepAssignment::FunctionSubroutineCall(_) => None,
        }
    }

    fn cond_predicate(&mut self, tree: &SyntaxTree, predicate: &sv::CondPredicate) -> UExpr {
        match predicate.nodes.0.contents().as_slice() {
            [sv::ExpressionOrCondPattern::Expression(expression)] => {
                self.expr(tree).expression(expression)
            }
            _ => self.expr(tree).unsupported_public(RefNode::CondPredicate(predicate)),
        }
    }

    fn case(&mut self, tree: &SyntaxTree, case: &sv::CaseStatement, span: Span) -> UStmt {
        let sv::CaseStatement::Normal(normal) = case else {
            // `case ... matches` and `case ... inside` are pattern forms.
            return self.unsupported_stmt(tree, RefNode::CaseStatement(case));
        };
        let (_unique, keyword, subject, first_item, rest, _endcase) = &normal.nodes;

        let case_kind = match keyword {
            sv::CaseKeyword::Case(_) => CaseKind::Case,
            sv::CaseKeyword::Casez(_) => CaseKind::Casez,
            // `casex` treats x in the *selector* as a wildcard too, which
            // silently masks real logic. RTLScope refuses to guess what it meant.
            sv::CaseKeyword::Casex(_) => {
                return self.unsupported_stmt(tree, RefNode::CaseStatementNormal(normal));
            }
        };

        let subject = self.expr(tree).expression(&subject.nodes.1.nodes.0);

        let mut arms = Vec::new();
        let mut default = None;
        for item in std::iter::once(first_item).chain(rest) {
            match item {
                sv::CaseItem::NonDefault(non_default) => {
                    let (labels, _colon, statement) = &non_default.nodes;
                    let labels = labels
                        .contents()
                        .into_iter()
                        .map(|label| self.expr(tree).expression(&label.nodes.0))
                        .collect();
                    let body = self.statement_or_null(tree, statement);
                    arms.push(UCaseArm { labels, body });
                }
                sv::CaseItem::Default(item) => {
                    let (_keyword, _colon, statement) = &item.nodes;
                    default = Some(self.statement_or_null(tree, statement));
                }
            }
        }

        UStmt::new(UStmtKind::Case { subject, case_kind, arms, default }, span)
    }

    // -------------------------------------------------------------- helpers ---

    /// The left-hand side of an assignment, as a name with any `[...]` applied.
    fn lvalue(&mut self, tree: &SyntaxTree, node: RefNode<'_>) -> UExpr {
        // `{a, b} = x` would have to be split into one assignment per part.
        if unwrap_node!(node.clone(), Concatenation, StreamingConcatenation).is_some() {
            return self.expr(tree).unsupported_public(node);
        }
        // `bus.valid[3:0] = x`: the name is everything before the brackets,
        // dots included — a signal of an interface is the net `bus.valid`.
        let Some(name) = nav::name_before_select(tree, node.clone()) else {
            return self.expr(tree).unsupported_public(node);
        };
        let span = self.span(tree, node.clone());
        let base = UExpr::ident(name, span);

        match unwrap_node!(node.clone(), Select, ConstantSelect) {
            Some(RefNode::Select(select)) => {
                self.expr(tree).apply_select_folded(base, select, node)
            }
            Some(RefNode::ConstantSelect(select)) => {
                self.expr(tree).apply_constant_select_folded(base, select, node)
            }
            _ => base,
        }
    }

    fn unsupported_stmt(&mut self, tree: &SyntaxTree, node: RefNode<'_>) -> UStmt {
        let construct = self.keyword_of(tree, node.clone());
        let span = self.span(tree, node);
        self.diags.push(
            rtlscope_ir::Diag::warning(
                rtlscope_ir::DiagCode::UnsupportedConstruct,
                format!("`{construct}` is outside the synthesisable subset and was skipped"),
            )
            .at(span),
        );
        UStmt::new(UStmtKind::Unsupported { construct }, span)
    }
}

/// The arithmetic behind a compound assignment: the `+` of `+=`.
fn compound_operator(text: &str) -> Option<BinOp> {
    Some(match text {
        "+=" => BinOp::Add,
        "-=" => BinOp::Sub,
        "*=" => BinOp::Mul,
        "/=" => BinOp::Div,
        "%=" => BinOp::Mod,
        "&=" => BinOp::BitAnd,
        "|=" => BinOp::BitOr,
        "^=" => BinOp::BitXor,
        "<<=" => BinOp::Shl,
        ">>=" => BinOp::Shr,
        ">>>=" => BinOp::AShr,
        _ => return None,
    })
}
