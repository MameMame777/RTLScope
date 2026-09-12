//! Operator precedence, which `sv-parser` does not apply.
//!
//! The parser builds a pure parse tree: `a == b && c == d` comes out as
//! `a == (b && (c == d))`, leaning entirely to the right. Nothing downstream
//! can catch it — the reads are the same signals whichever way it is grouped —
//! so the error surfaces much later as a constant folded wrong or a condition
//! that reads backwards. These tests pin the shape of the tree itself.

use rtlscope_ir::{UExpr, UExprKind, UItem, UModule};
use rtlscope_sv::ParseOptions;

fn assigns(module: &UModule) -> Vec<(String, String)> {
    module
        .items
        .iter()
        .filter_map(|item| match item {
            UItem::Assign { lhs, rhs, .. } => Some((render(lhs), render(rhs))),
            _ => None,
        })
        .collect()
}

/// Fully parenthesised, so the grouping is the thing being asserted.
fn render(expr: &UExpr) -> String {
    match &*expr.kind {
        UExprKind::Ident { name } => name.clone(),
        UExprKind::Binary { op, lhs, rhs } => {
            format!("({} {:?} {})", render(lhs), op, render(rhs))
        }
        UExprKind::Unary { op, operand } => format!("{op:?}{}", render(operand)),
        other => format!("{other:?}"),
    }
}

fn precedence_module() -> UModule {
    let path = rtlscope_fixtures::path("precedence.sv");
    let (design, diags) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    assert!(!diags.has_errors(), "{diags:?}");
    design.modules.into_iter().find(|m| m.name == "precedence").expect("module `precedence`")
}

#[test]
fn a_run_of_operators_is_grouped_by_precedence() {
    let module = precedence_module();
    let assigns = assigns(&module);
    let of = |name: &str| {
        assigns
            .iter()
            .find(|(lhs, _)| lhs == name)
            .map(|(_, rhs)| rhs.clone())
            .unwrap_or_else(|| panic!("no assignment to `{name}`; have {assigns:?}"))
    };

    // The one that started this: `==` binds tighter than `&&`.
    assert_eq!(of("eq_and"), "((a Eq b) LogAnd (c Eq d))");
    assert_eq!(of("add_mul"), "(a Add (b Mul c))");
    assert_eq!(of("or_and"), "(a BitOr (b BitAnd c))");
    assert_eq!(of("shift_add"), "((a Add b) Shl c)");
}

/// Operators of equal precedence group leftwards, as IEEE 1800 says.
#[test]
fn equal_precedence_groups_to_the_left() {
    let module = precedence_module();
    let assigns = assigns(&module);
    let chain = assigns.iter().find(|(lhs, _)| lhs == "cmp_chain").expect("cmp_chain").1.clone();
    // `a < b == c` is `(a < b) == c`: `<` binds tighter, and what is left
    // associates leftwards.
    assert_eq!(chain, "((a Lt b) Eq c)");
}

/// Parentheses in the source survive: they are a `Primary`, not a binary node,
/// so the run being flattened stops at them.
#[test]
fn parentheses_are_not_flattened_away() {
    let module = precedence_module();
    let assigns = assigns(&module);
    let value =
        assigns.iter().find(|(lhs, _)| lhs == "parenthesised").expect("parenthesised").1.clone();
    assert_eq!(value, "((a Eq b) Eq (c Eq d))");
}
