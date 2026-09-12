//! Copying a function body into the place it is called.
//!
//! A function is not hardware. `sat(a)` and `sat(b)` are two separate pieces of
//! logic that happen to have been written once, and synthesis makes a copy of
//! the body for each call. Elaboration does the same thing here, so that
//! everything downstream — reads/writes derivation, the block diagram, the
//! pipeline graph — sees only the logic, and never has to know what a function
//! is.
//!
//! The copy is made by renaming: every argument, local, and the function's own
//! name (which is how SystemVerilog spells the return value) is rewritten to a
//! call-site-specific name before the body is lowered. Renaming the *unresolved*
//! IR rather than the elaborated one is what keeps this small — one pass over a
//! tree of names, with no nets, widths or resolution to think about.

use std::collections::HashMap;

use rtlscope_ir::{UCaseArm, UExpr, UExprKind, UStmt, UStmtKind};

/// Names to substitute while copying a function body.
pub(crate) type Renames = HashMap<String, String>;

pub(crate) fn rename_stmt(stmt: &UStmt, renames: &Renames) -> UStmt {
    let span = stmt.span;
    let kind = match &*stmt.kind {
        UStmtKind::Block { stmts, decls } => UStmtKind::Block {
            stmts: stmts.iter().map(|s| rename_stmt(s, renames)).collect(),
            // A block inside a function has had its declarations hoisted to the
            // function already, so there is nothing here to rename.
            decls: decls.clone(),
        },
        UStmtKind::Assign { lhs, rhs, blocking } => UStmtKind::Assign {
            lhs: rename_expr(lhs, renames),
            rhs: rename_expr(rhs, renames),
            blocking: *blocking,
        },
        UStmtKind::If { cond, then_branch, else_branch } => UStmtKind::If {
            cond: rename_expr(cond, renames),
            then_branch: rename_stmt(then_branch, renames),
            else_branch: else_branch.as_ref().map(|s| rename_stmt(s, renames)),
        },
        UStmtKind::Case { subject, case_kind, arms, default } => UStmtKind::Case {
            subject: rename_expr(subject, renames),
            case_kind: *case_kind,
            arms: arms
                .iter()
                .map(|UCaseArm { labels, body }| UCaseArm {
                    labels: labels.iter().map(|l| rename_expr(l, renames)).collect(),
                    body: rename_stmt(body, renames),
                })
                .collect(),
            default: default.as_ref().map(|s| rename_stmt(s, renames)),
        },
        UStmtKind::For { var, init, cond, step, body } => UStmtKind::For {
            // The loop variable is local to the loop, and the caller's scope
            // cannot see it, so it keeps its name.
            var: var.clone(),
            init: rename_expr(init, renames),
            cond: rename_expr(cond, renames),
            step: rename_expr(step, renames),
            body: rename_stmt(body, renames),
        },
        UStmtKind::While { cond, body } => UStmtKind::While {
            cond: rename_expr(cond, renames),
            body: Box::new(rename_stmt(body, renames)),
        },
        UStmtKind::Call { name, args } => UStmtKind::Call {
            name: name.clone(),
            args: args.iter().map(|a| rename_expr(a, renames)).collect(),
        },
        UStmtKind::Unsupported { construct } => {
            UStmtKind::Unsupported { construct: construct.clone() }
        }
    };
    UStmt::new(kind, span)
}

pub(crate) fn rename_expr(expr: &UExpr, renames: &Renames) -> UExpr {
    let span = expr.span;
    let kind = match &*expr.kind {
        UExprKind::Ident { name } => {
            let name = renames.get(name).cloned().unwrap_or_else(|| name.clone());
            UExprKind::Ident { name }
        }
        // A literal has no names in it, and nor does an unsupported hole — its
        // text is there to be reported, not resolved.
        UExprKind::Int { .. }
        | UExprKind::Sized { .. }
        | UExprKind::Fill { .. }
        | UExprKind::Unsupported { .. } => return expr.clone(),
        UExprKind::Unary { op, operand } => {
            UExprKind::Unary { op: *op, operand: rename_expr(operand, renames) }
        }
        UExprKind::Binary { op, lhs, rhs } => UExprKind::Binary {
            op: *op,
            lhs: rename_expr(lhs, renames),
            rhs: rename_expr(rhs, renames),
        },
        UExprKind::Ternary { cond, then_value, else_value } => UExprKind::Ternary {
            cond: rename_expr(cond, renames),
            then_value: rename_expr(then_value, renames),
            else_value: rename_expr(else_value, renames),
        },
        UExprKind::Concat { parts } => {
            UExprKind::Concat { parts: parts.iter().map(|p| rename_expr(p, renames)).collect() }
        }
        UExprKind::Repl { count, value } => UExprKind::Repl {
            count: rename_expr(count, renames),
            value: rename_expr(value, renames),
        },
        UExprKind::Index { base, index } => UExprKind::Index {
            base: rename_expr(base, renames),
            index: rename_expr(index, renames),
        },
        UExprKind::Range { base, msb, lsb } => UExprKind::Range {
            base: rename_expr(base, renames),
            msb: rename_expr(msb, renames),
            lsb: rename_expr(lsb, renames),
        },
        UExprKind::Clog2 { arg } => UExprKind::Clog2 { arg: rename_expr(arg, renames) },
        UExprKind::Cast { width, value } => UExprKind::Cast {
            width: rename_expr(width, renames),
            value: rename_expr(value, renames),
        },
        // The callee is a function name, not a variable, so it is left alone;
        // only the arguments are in the caller's world.
        UExprKind::Call { name, args } => UExprKind::Call {
            name: name.clone(),
            args: args.iter().map(|a| rename_expr(a, renames)).collect(),
        },
    };
    UExpr::new(kind, span)
}
