//! SystemVerilog expressions to [`UExpr`].
//!
//! Two things about the grammar shape this module.
//!
//! First, `ConstantExpression` and `Expression` are separate production trees
//! with parallel structure, so most operators are handled twice. They are kept
//! separate rather than merged because a constant expression is the only thing
//! allowed in a width or a parameter default, and blurring the two would let a
//! non-constant slip into a place elaboration cannot evaluate.
//!
//! Second, a bare identifier inside a constant expression does *not* parse as an
//! identifier. `parameter int W = 8; logic [W-1:0] x;` gives `W` as
//! `ConstantPrimary::ConstantFunctionCall` — a zero-argument function call —
//! because the grammar cannot tell the two apart without knowing what `W` is.
//! Undoing that is [`ExprLower::subroutine_call`], and forgetting it would make
//! every parameter reference in every width look like a call.
//!
//! Anything outside the subset becomes `UExprKind::Unsupported` carrying the
//! author's own text, never a guess and never a silent drop (D2).

use rtlscope_ir::{BinOp, Span, UExpr, UExprKind, UnOp};
use sv_parser::{self as sv, RefNode, SyntaxTree};

use crate::nav;
use crate::number::parse_number;
use crate::source_map::SourceMap;

pub(crate) struct ExprLower<'a, 'm> {
    pub tree: &'a SyntaxTree,
    pub map: &'m mut SourceMap,
}

impl ExprLower<'_, '_> {
    pub fn span(&mut self, node: RefNode<'_>) -> Span {
        match nav::first_token(node) {
            Some(locate) => self.map.resolve(self.tree, &locate),
            None => Span::UNKNOWN,
        }
    }

    /// For callers outside this module that hit a construct the subset excludes.
    pub fn unsupported_public(&mut self, node: RefNode<'_>) -> UExpr {
        self.unsupported(node)
    }

    fn unsupported(&mut self, node: RefNode<'_>) -> UExpr {
        let text = nav::subtree_text(self.tree, node.clone());
        let span = self.span(node);
        UExpr::new(UExprKind::Unsupported { text }, span)
    }

    fn token_text(&self, node: RefNode<'_>) -> String {
        nav::first_token(node)
            .and_then(|locate| self.tree.get_str(&locate).map(str::to_owned))
            .unwrap_or_default()
    }

    // ------------------------------------------------------ constant side ---

    pub fn constant_expression(&mut self, x: &sv::ConstantExpression) -> UExpr {
        match x {
            sv::ConstantExpression::ConstantPrimary(p) => self.constant_primary(p),
            sv::ConstantExpression::Unary(u) => {
                let (operator, _attrs, primary) = &u.nodes;
                let operand = self.constant_primary(primary);
                self.apply_unary(
                    RefNode::UnaryOperator(operator),
                    operand,
                    RefNode::ConstantExpressionUnary(u),
                )
            }
            sv::ConstantExpression::Binary(b) => self.constant_chain(b),
            sv::ConstantExpression::Ternary(t) => {
                let (cond, _q, _attrs, then_value, _colon, else_value) = &t.nodes;
                let cond = self.constant_expression(cond);
                let then_value = self.constant_expression(then_value);
                let else_value = self.constant_expression(else_value);
                let span = self.span(RefNode::ConstantExpressionTernary(t));
                UExpr::new(UExprKind::Ternary { cond, then_value, else_value }, span)
            }
            sv::ConstantExpression::Inside(i) => {
                self.unsupported(RefNode::ConstantInsideExpression(i))
            }
        }
    }

    fn constant_primary(&mut self, x: &sv::ConstantPrimary) -> UExpr {
        match x {
            sv::ConstantPrimary::PrimaryLiteral(l) => self.literal(RefNode::PrimaryLiteral(l)),
            sv::ConstantPrimary::ConstantFunctionCall(c) => {
                let sv::ConstantFunctionCall { nodes: (call,) } = &**c;
                let sv::FunctionSubroutineCall { nodes: (call,) } = call;
                self.subroutine_call(call, RefNode::ConstantFunctionCall(c))
            }
            sv::ConstantPrimary::PsParameter(p) => {
                // `W` where the grammar could tell it was a parameter, and
                // `pkg::W`. A trailing select is not in the subset yet.
                let (identifier, select) = &p.nodes;
                let base = self.identifier_expr(
                    RefNode::PsParameterIdentifier(identifier),
                    RefNode::ConstantPrimaryPsParameter(p),
                );
                self.apply_constant_select(base, select, RefNode::ConstantPrimaryPsParameter(p))
            }
            sv::ConstantPrimary::ConstantCast(cast) => {
                let (casting_type, _tick, paren) = &cast.nodes;
                let Some(width) = self.cast_width(casting_type) else {
                    return self.unsupported(RefNode::ConstantCast(cast));
                };
                let value = self.constant_expression(&paren.nodes.1);
                let span = self.span(RefNode::ConstantCast(cast));
                UExpr::new(UExprKind::Cast { width, value }, span)
            }
            sv::ConstantPrimary::GenvarIdentifier(g) => {
                self.identifier_expr(RefNode::GenvarIdentifier(g), RefNode::GenvarIdentifier(g))
            }
            sv::ConstantPrimary::MintypmaxExpression(m) => {
                // A parenthesised expression.
                let sv::ConstantPrimaryMintypmaxExpression { nodes: (paren,) } = &**m;
                let (_open, inner, _close) = &paren.nodes;
                match inner {
                    sv::ConstantMintypmaxExpression::Unary(u) => self.constant_expression(u),
                    sv::ConstantMintypmaxExpression::Ternary(_) => {
                        self.unsupported(RefNode::ConstantPrimaryMintypmaxExpression(m))
                    }
                }
            }
            sv::ConstantPrimary::Concatenation(c) => {
                let (concat, range) = &c.nodes;
                if range.is_some() {
                    return self.unsupported(RefNode::ConstantPrimaryConcatenation(c));
                }
                self.constant_concatenation(concat, RefNode::ConstantPrimaryConcatenation(c))
            }
            sv::ConstantPrimary::MultipleConcatenation(m) => {
                let (concat, range) = &m.nodes;
                if range.is_some() {
                    return self.unsupported(RefNode::ConstantPrimaryMultipleConcatenation(m));
                }
                let (_open, (count, inner), _close) = &concat.nodes.0.nodes;
                let count = self.constant_expression(count);
                let value =
                    self.constant_concatenation(inner, RefNode::ConstantConcatenation(inner));
                let span = self.span(RefNode::ConstantPrimaryMultipleConcatenation(m));
                UExpr::new(UExprKind::Repl { count, value }, span)
            }
            other => self.unsupported(RefNode::ConstantPrimary(other)),
        }
    }

    // --------------------------------------------------- non-constant side ---

    pub fn expression(&mut self, x: &sv::Expression) -> UExpr {
        match x {
            sv::Expression::Primary(p) => self.primary(p),
            sv::Expression::Unary(u) => {
                let (operator, _attrs, primary) = &u.nodes;
                let operand = self.primary(primary);
                self.apply_unary(
                    RefNode::UnaryOperator(operator),
                    operand,
                    RefNode::ExpressionUnary(u),
                )
            }
            sv::Expression::Binary(b) => self.chain(b),
            sv::Expression::ConditionalExpression(c) => {
                let (cond, _q, _attrs, then_value, _colon, else_value) = &c.nodes;
                let cond = self.cond_predicate(cond);
                let then_value = self.expression(then_value);
                let else_value = self.expression(else_value);
                let span = self.span(RefNode::ConditionalExpression(c));
                UExpr::new(UExprKind::Ternary { cond, then_value, else_value }, span)
            }
            other => self.unsupported(RefNode::Expression(other)),
        }
    }

    fn cond_predicate(&mut self, x: &sv::CondPredicate) -> UExpr {
        // `a ? b : c` — the predicate is a list of pattern-matched expressions
        // in full SystemVerilog, but a plain expression in synthesisable code.
        let items = x.nodes.0.contents();
        match items.as_slice() {
            [sv::ExpressionOrCondPattern::Expression(e)] => self.expression(e),
            _ => self.unsupported(RefNode::CondPredicate(x)),
        }
    }

    fn primary(&mut self, x: &sv::Primary) -> UExpr {
        match x {
            sv::Primary::PrimaryLiteral(l) => self.literal(RefNode::PrimaryLiteral(l)),
            sv::Primary::Hierarchical(h) => {
                let (scope, identifier, select) = &h.nodes;
                // These two `Option`/container nodes are `Some` even for a bare
                // identifier — they are simply empty. Emptiness has to be tested
                // by looking for a token, not by `is_some`, or every plain
                // signal name reads as a package-qualified select.
                let has_scope = scope.as_ref().is_some_and(|scope| {
                    nav::first_token(RefNode::ClassQualifierOrPackageScope(scope)).is_some()
                });
                // `defs::IDLE` is a name in a package, and is carried as the
                // whole spelling. A class scope is outside the subset.
                let package = scope.as_ref().and_then(|scope| {
                    nav::package_of(self.tree, RefNode::ClassQualifierOrPackageScope(scope))
                });
                if has_scope && package.is_none() {
                    return self.unsupported(RefNode::PrimaryHierarchical(h));
                }
                let base = self.identifier_expr(
                    RefNode::HierarchicalIdentifier(identifier),
                    RefNode::PrimaryHierarchical(h),
                );
                let base = match package {
                    Some(package) => qualify(base, &package),
                    None => base,
                };
                self.apply_select(base, select, RefNode::PrimaryHierarchical(h))
            }
            sv::Primary::FunctionSubroutineCall(f) => {
                let sv::FunctionSubroutineCall { nodes: (call,) } = &**f;
                self.subroutine_call(call, RefNode::FunctionSubroutineCall(f))
            }
            sv::Primary::Cast(cast) => {
                let (casting_type, _tick, paren) = &cast.nodes;
                let Some(width) = self.cast_width(casting_type) else {
                    return self.unsupported(RefNode::Cast(cast));
                };
                let value = self.expression(&paren.nodes.1);
                let span = self.span(RefNode::Cast(cast));
                UExpr::new(UExprKind::Cast { width, value }, span)
            }
            sv::Primary::MintypmaxExpression(m) => {
                let sv::PrimaryMintypmaxExpression { nodes: (paren,) } = &**m;
                let (_open, inner, _close) = &paren.nodes;
                match inner {
                    sv::MintypmaxExpression::Expression(e) => self.expression(e),
                    sv::MintypmaxExpression::Ternary(_) => {
                        self.unsupported(RefNode::PrimaryMintypmaxExpression(m))
                    }
                }
            }
            sv::Primary::Concatenation(c) => {
                let (concat, range) = &c.nodes;
                if range.is_some() {
                    return self.unsupported(RefNode::PrimaryConcatenation(c));
                }
                self.concatenation(concat, RefNode::PrimaryConcatenation(c))
            }
            sv::Primary::MultipleConcatenation(m) => {
                let (concat, range) = &m.nodes;
                if range.is_some() {
                    return self.unsupported(RefNode::PrimaryMultipleConcatenation(m));
                }
                self.multiple_concatenation(concat, RefNode::PrimaryMultipleConcatenation(m))
            }
            other => self.unsupported(RefNode::Primary(other)),
        }
    }

    // ------------------------------------------------------------ shared ---

    /// Undoes the grammar's habit of parsing a bare identifier as a call, and
    /// picks out the one system function the subset allows.
    fn subroutine_call(&mut self, call: &sv::SubroutineCall, whole: RefNode<'_>) -> UExpr {
        match call {
            sv::SubroutineCall::TfCall(t) => {
                let (identifier, _attrs, args) = &t.nodes;
                match args {
                    // No argument list: this was never a call, just a name.
                    None => self
                        .identifier_expr(RefNode::PsOrHierarchicalTfIdentifier(identifier), whole),
                    Some(paren) => {
                        let name_node = RefNode::PsOrHierarchicalTfIdentifier(identifier);
                        let Some(name) = nav::name_str(self.tree, name_node) else {
                            return self.unsupported(whole);
                        };
                        let Some(args) = self.ordered_arguments(&paren.nodes.1) else {
                            return self.unsupported(whole);
                        };
                        let span = self.span(whole);
                        UExpr::new(UExprKind::Call { name, args }, span)
                    }
                }
            }
            sv::SubroutineCall::SystemTfCall(s) => {
                // `$clog2(x)` takes two different shapes depending on how the
                // grammar resolved the argument list, and only one of them is
                // the obvious `ArgOptionl`.
                let (identifier, argument) = match &**s {
                    sv::SystemTfCall::ArgOptionl(call) => {
                        let (identifier, args) = &call.nodes;
                        let argument =
                            args.as_ref().and_then(|paren| self.single_argument(&paren.nodes.1));
                        (identifier, argument)
                    }
                    sv::SystemTfCall::ArgExpression(call) => {
                        let (identifier, paren) = &call.nodes;
                        let (arguments, clocking) = &paren.nodes.1;
                        if clocking.is_some() {
                            return self.unsupported(whole);
                        }
                        let argument = match arguments.contents().as_slice() {
                            [Some(expression)] => Some(self.expression(expression)),
                            _ => None,
                        };
                        (identifier, argument)
                    }
                    sv::SystemTfCall::ArgDataType(_) => return self.unsupported(whole),
                };

                let name = self.token_text(RefNode::SystemTfIdentifier(identifier));
                if !name.eq_ignore_ascii_case("$clog2") {
                    return self.unsupported(whole);
                }
                let Some(arg) = argument else { return self.unsupported(whole) };
                let span = self.span(whole);
                UExpr::new(UExprKind::Clog2 { arg }, span)
            }
            _ => self.unsupported(whole),
        }
    }

    /// Every argument of a positional call, or `None` if any of them is named
    /// or omitted — both of which would make the mapping to parameters guesswork.
    pub(crate) fn ordered_arguments(&mut self, args: &sv::ListOfArguments) -> Option<Vec<UExpr>> {
        let sv::ListOfArguments::Ordered(ordered) = args else { return None };
        let (list, named) = &ordered.nodes;
        if !named.is_empty() {
            return None;
        }
        list.contents()
            .iter()
            .map(|argument| argument.as_ref().map(|e| self.expression(e)))
            .collect()
    }

    /// The sole expression of a one-argument call, or `None` if the call takes
    /// any other shape.
    fn single_argument(&mut self, args: &sv::ListOfArguments) -> Option<UExpr> {
        let sv::ListOfArguments::Ordered(ordered) = args else { return None };
        let (list, named) = &ordered.nodes;
        if !named.is_empty() {
            return None;
        }
        match list.contents().as_slice() {
            [Some(expression)] => Some(self.expression(expression)),
            _ => None,
        }
    }

    fn concatenation(&mut self, concat: &sv::Concatenation, whole: RefNode<'_>) -> UExpr {
        let (_open, list, _close) = &concat.nodes.0.nodes;
        let parts =
            list.contents().into_iter().map(|expression| self.expression(expression)).collect();
        let span = self.span(whole);
        UExpr::new(UExprKind::Concat { parts }, span)
    }

    fn constant_concatenation(
        &mut self,
        concat: &sv::ConstantConcatenation,
        whole: RefNode<'_>,
    ) -> UExpr {
        let (_open, list, _close) = &concat.nodes.0.nodes;
        let parts = list
            .contents()
            .into_iter()
            .map(|expression| self.constant_expression(expression))
            .collect();
        let span = self.span(whole);
        UExpr::new(UExprKind::Concat { parts }, span)
    }

    fn multiple_concatenation(
        &mut self,
        concat: &sv::MultipleConcatenation,
        whole: RefNode<'_>,
    ) -> UExpr {
        let (_open, (count, inner), _close) = &concat.nodes.0.nodes;
        let count = self.expression(count);
        let value = self.concatenation(inner, RefNode::Concatenation(inner));
        let span = self.span(whole);
        UExpr::new(UExprKind::Repl { count, value }, span)
    }

    /// Wraps a base expression in whatever `[...]` follows it.
    ///
    /// `mem[addr]` is one bit select; `wr_ptr[AW-1:0]` is a part select;
    /// `mem[i][3:0]` is both. Member access (`s.field`) and indexed ranges
    /// (`x[base +: 8]`) are outside the subset and reported.
    pub(crate) fn apply_select(
        &mut self,
        base: UExpr,
        select: &sv::Select,
        whole: RefNode<'_>,
    ) -> UExpr {
        self.apply_select_in(base, select, whole, false)
    }

    /// The same, for a target whose `.member` chain was already read into its
    /// name — `bus.valid` — so only the brackets are left to apply.
    pub(crate) fn apply_select_folded(
        &mut self,
        base: UExpr,
        select: &sv::Select,
        whole: RefNode<'_>,
    ) -> UExpr {
        self.apply_select_in(base, select, whole, true)
    }

    fn apply_select_in(
        &mut self,
        base: UExpr,
        select: &sv::Select,
        whole: RefNode<'_>,
        members_folded: bool,
    ) -> UExpr {
        let (member_chain, bit_select, part_select) = &select.nodes;
        if member_chain.is_some() && !members_folded {
            return self.unsupported(whole);
        }

        let mut result = base;
        for bracket in &bit_select.nodes.0 {
            let index = self.expression(&bracket.nodes.1);
            let span = index.span;
            result = UExpr::new(UExprKind::Index { base: result, index }, span);
        }

        let Some(bracket) = part_select else { return result };
        match &bracket.nodes.1 {
            sv::PartSelectRange::ConstantRange(range) => {
                let (msb, _colon, lsb) = &range.nodes;
                let msb = self.constant_expression(msb);
                let lsb = self.constant_expression(lsb);
                let span = msb.span;
                UExpr::new(UExprKind::Range { base: result, msb, lsb }, span)
            }
            sv::PartSelectRange::IndexedRange(range) => {
                let (base_expr, symbol, width) = &range.nodes;
                let offset = self.expression(base_expr);
                let width = self.constant_expression(width);
                self.indexed_range(result, offset, width, symbol, whole)
            }
        }
    }

    /// The constant-expression twin of [`ExprLower::apply_select`].
    pub(crate) fn apply_constant_select(
        &mut self,
        base: UExpr,
        select: &sv::ConstantSelect,
        whole: RefNode<'_>,
    ) -> UExpr {
        self.apply_constant_select_in(base, select, whole, false)
    }

    /// See [`ExprLower::apply_select_folded`].
    pub(crate) fn apply_constant_select_folded(
        &mut self,
        base: UExpr,
        select: &sv::ConstantSelect,
        whole: RefNode<'_>,
    ) -> UExpr {
        self.apply_constant_select_in(base, select, whole, true)
    }

    fn apply_constant_select_in(
        &mut self,
        base: UExpr,
        select: &sv::ConstantSelect,
        whole: RefNode<'_>,
        members_folded: bool,
    ) -> UExpr {
        let (member_chain, bit_select, part_select) = &select.nodes;
        if member_chain.is_some() && !members_folded {
            return self.unsupported(whole);
        }

        let mut result = base;
        for bracket in &bit_select.nodes.0 {
            let index = self.constant_expression(&bracket.nodes.1);
            let span = index.span;
            result = UExpr::new(UExprKind::Index { base: result, index }, span);
        }

        let Some(bracket) = part_select else { return result };
        match &bracket.nodes.1 {
            sv::ConstantPartSelectRange::ConstantRange(range) => {
                let (msb, _colon, lsb) = &range.nodes;
                let msb = self.constant_expression(msb);
                let lsb = self.constant_expression(lsb);
                let span = msb.span;
                UExpr::new(UExprKind::Range { base: result, msb, lsb }, span)
            }
            sv::ConstantPartSelectRange::ConstantIndexedRange(range) => {
                let (base_expr, symbol, width) = &range.nodes;
                let offset = self.constant_expression(base_expr);
                let width = self.constant_expression(width);
                self.indexed_range(result, offset, width, symbol, whole)
            }
        }
    }

    /// `x[offset +: width]` and `x[offset -: width]`, rewritten as the plain
    /// `[msb:lsb]` the IR carries.
    ///
    /// The width is constant by the grammar, so the two forms differ only in
    /// which end the offset names — which makes this arithmetic rather than a
    /// new kind of select.
    fn indexed_range(
        &mut self,
        base: UExpr,
        offset: UExpr,
        width: UExpr,
        symbol: &sv::Symbol,
        whole: RefNode<'_>,
    ) -> UExpr {
        let span = offset.span;
        let one = UExpr::int(1, span);
        // `width - 1` is the distance from the offset to the far end.
        let reach = UExpr::new(UExprKind::Binary { op: BinOp::Sub, lhs: width, rhs: one }, span);
        let (msb, lsb) = match self.token_text(RefNode::Symbol(symbol)).as_str() {
            "+:" => (
                UExpr::new(
                    UExprKind::Binary { op: BinOp::Add, lhs: offset.clone(), rhs: reach },
                    span,
                ),
                offset,
            ),
            "-:" => (
                offset.clone(),
                UExpr::new(UExprKind::Binary { op: BinOp::Sub, lhs: offset, rhs: reach }, span),
            ),
            _ => return self.unsupported(whole),
        };
        UExpr::new(UExprKind::Range { base, msb, lsb }, span)
    }

    /// The width of `16'(...)`, which is a constant expression in front of the
    /// tick. A type cast — `logic'(x)`, `state_e'(y)` — is a different thing and
    /// is not in the subset.
    fn cast_width(&mut self, casting_type: &sv::CastingType) -> Option<UExpr> {
        match casting_type {
            sv::CastingType::ConstantPrimary(primary) => Some(self.constant_primary(primary)),
            _ => None,
        }
    }

    fn literal(&mut self, node: RefNode<'_>) -> UExpr {
        let text = nav::subtree_text(self.tree, node.clone());
        match parse_number(&text) {
            Some(kind) => {
                let span = self.span(node);
                UExpr::new(kind, span)
            }
            None => self.unsupported(node),
        }
    }

    pub(crate) fn identifier_expr(&mut self, name_node: RefNode<'_>, whole: RefNode<'_>) -> UExpr {
        match nav::name_str(self.tree, name_node) {
            Some(name) => {
                let span = self.span(whole);
                UExpr::new(UExprKind::Ident { name }, span)
            }
            None => self.unsupported(whole),
        }
    }

    /// `a == b && c == d`, put back together the way SystemVerilog means it.
    ///
    /// `sv-parser` builds a pure syntax tree and applies no precedence at all:
    /// a run of binary operators comes out leaning entirely to the right, so
    /// that expression arrives as `a == (b && (c == d))`. Lowering it as it
    /// stands does not merely misprint the expression — it folds constants and
    /// evaluates conditions against the wrong tree, quietly.
    ///
    /// So the run is flattened back into operands and operators and rebuilt by
    /// precedence. Anything the author parenthesised is a `Primary` rather than
    /// a binary node, so the flattening stops at it and the grouping is kept.
    fn chain(&mut self, b: &sv::ExpressionBinary) -> UExpr {
        let mut operands = Vec::new();
        let mut operators = Vec::new();
        let mut current = b;
        loop {
            let (lhs, operator, _attrs, rhs) = &current.nodes;
            operands.push(self.expression(lhs));
            operators.push(self.operator(RefNode::BinaryOperator(operator)));
            match rhs {
                sv::Expression::Binary(next) => current = next,
                other => {
                    operands.push(self.expression(other));
                    break;
                }
            }
        }
        reassociate(operands, operators)
    }

    /// The constant-expression twin of [`ExprLower::chain`].
    fn constant_chain(&mut self, b: &sv::ConstantExpressionBinary) -> UExpr {
        let mut operands = Vec::new();
        let mut operators = Vec::new();
        let mut current = b;
        loop {
            let (lhs, operator, _attrs, rhs) = &current.nodes;
            operands.push(self.constant_expression(lhs));
            operators.push(self.operator(RefNode::BinaryOperator(operator)));
            match rhs {
                sv::ConstantExpression::Binary(next) => current = next,
                other => {
                    operands.push(self.constant_expression(other));
                    break;
                }
            }
        }
        reassociate(operands, operators)
    }

    fn operator(&mut self, node: RefNode<'_>) -> Operator {
        let span = self.span(node.clone());
        let text = self.token_text(node);
        Operator { level: precedence_of(&text), text, span }
    }

    fn apply_unary(&mut self, operator: RefNode<'_>, operand: UExpr, whole: RefNode<'_>) -> UExpr {
        let text = self.token_text(operator);
        // Unary `+` is the identity; there is nothing to record.
        if text == "+" {
            return operand;
        }
        match to_unary_op(&text) {
            Some(op) => {
                let span = self.span(whole);
                UExpr::new(UExprKind::Unary { op, operand }, span)
            }
            None => self.unsupported(whole),
        }
    }
}

fn to_unary_op(text: &str) -> Option<UnOp> {
    Some(match text {
        "~" => UnOp::BitNot,
        "!" => UnOp::LogNot,
        "-" => UnOp::Neg,
        "&" => UnOp::RedAnd,
        "|" => UnOp::RedOr,
        "^" => UnOp::RedXor,
        _ => return None,
    })
}

fn to_binary_op(text: &str) -> Option<BinOp> {
    Some(match text {
        "+" => BinOp::Add,
        "-" => BinOp::Sub,
        "*" => BinOp::Mul,
        "/" => BinOp::Div,
        "%" => BinOp::Mod,
        "&" => BinOp::BitAnd,
        "|" => BinOp::BitOr,
        "^" => BinOp::BitXor,
        "~^" | "^~" => BinOp::BitXnor,
        // `<<<` on an unsigned value is `<<`; the distinction is in the operand.
        "<<" | "<<<" => BinOp::Shl,
        ">>" => BinOp::Shr,
        ">>>" => BinOp::AShr,
        "==" => BinOp::Eq,
        "!=" => BinOp::Ne,
        "<" => BinOp::Lt,
        "<=" => BinOp::Le,
        ">" => BinOp::Gt,
        ">=" => BinOp::Ge,
        "&&" => BinOp::LogAnd,
        "||" => BinOp::LogOr,
        // `===` and `!==` compare x and z, which synthesis cannot honour.
        _ => return None,
    })
}

/// A binary operator, with how tightly it binds.
struct Operator {
    text: String,
    /// Its row in IEEE 1800 table 11-2. Zero for an operator outside the
    /// subset, which then binds loosest and becomes a reported hole rather than
    /// a silently regrouped expression.
    level: u8,
    span: Span,
}

/// Rebuilds a flattened run of operators into a tree.
///
/// Two stacks, and every SystemVerilog binary operator is left-associative, so
/// an incoming operator that binds no tighter than the one on top reduces
/// first: `a - b - c` becomes `(a - b) - c`.
fn reassociate(operands: Vec<UExpr>, operators: Vec<Operator>) -> UExpr {
    debug_assert_eq!(operands.len(), operators.len() + 1);
    let mut operands = operands.into_iter();
    let mut values = vec![operands.next().expect("a run has at least one operand")];
    let mut pending: Vec<Operator> = Vec::new();

    for (operator, rhs) in operators.into_iter().zip(operands) {
        while pending.last().is_some_and(|top| top.level >= operator.level) {
            reduce(&mut values, &mut pending);
        }
        pending.push(operator);
        values.push(rhs);
    }
    while !pending.is_empty() {
        reduce(&mut values, &mut pending);
    }
    values.pop().expect("one value is left when every operator has been applied")
}

fn reduce(values: &mut Vec<UExpr>, pending: &mut Vec<Operator>) {
    let operator = pending.pop().expect("reduce is only called with an operator to apply");
    let rhs = values.pop().expect("an operator has a right operand");
    let lhs = values.pop().expect("an operator has a left operand");
    let value = match to_binary_op(&operator.text) {
        Some(op) => UExpr::new(UExprKind::Binary { op, lhs, rhs }, operator.span),
        None => UExpr::new(UExprKind::Unsupported { text: operator.text }, operator.span),
    };
    values.push(value);
}

/// Where an operator sits in IEEE 1800 table 11-2, tightest highest.
///
/// Only what the subset can build is placed. An operator outside it gets zero,
/// which puts it at the root of whatever run it is in, where the hole it leaves
/// is reported against the whole expression rather than hidden inside it.
fn precedence_of(text: &str) -> u8 {
    match text {
        "**" => 11,
        "*" | "/" | "%" => 10,
        "+" | "-" => 9,
        "<<" | ">>" | "<<<" | ">>>" => 8,
        "<" | "<=" | ">" | ">=" => 7,
        "==" | "!=" | "===" | "!==" | "==?" | "!=?" => 6,
        "&" => 5,
        "^" | "^~" | "~^" => 4,
        "|" => 3,
        "&&" => 2,
        "||" => 1,
        _ => 0,
    }
}

/// `defs::WIDTH` from a lowered `WIDTH` and the package it was qualified with.
///
/// In the grammar node for a primary, the scope is a sibling of the name rather
/// than part of it, so the name is lowered on its own and the package put in
/// front afterwards. Anything that is not a plain name is left as it is.
fn qualify(expr: UExpr, package: &str) -> UExpr {
    let span = expr.span;
    match *expr.kind {
        UExprKind::Ident { name } => {
            UExpr::new(UExprKind::Ident { name: nav::qualified(Some(package), &name) }, span)
        }
        other => UExpr::new(other, span),
    }
}
