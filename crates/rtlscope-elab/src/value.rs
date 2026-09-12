//! Constant folding.
//!
//! Every width, array bound, parameter value and generate condition passes
//! through here. The subset is integer arithmetic on `i64` plus `$clog2`, which
//! is the one system function width arithmetic cannot be written without.
//!
//! An expression that cannot be folded is an error, never a guess: a design
//! whose widths were assumed would draw a diagram that looks right and is
//! wrong. The caller turns [`EvalError`] into a diagnostic pointing at the
//! offending expression.

use std::collections::HashMap;

use rtlscope_ir::{BinOp, Span, UExpr, UExprKind, UnOp};

/// Named integer constants in scope: parameters, localparams and the genvar of
/// any enclosing generate loop.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Value, and the declared width when there is one.
    ///
    /// The width only matters inside a concatenation, which is the one place a
    /// constant's width changes the answer: `{VC, DT}` puts `DT` in the low six
    /// bits only if six is known.
    values: HashMap<String, (i64, Option<u32>)>,
}

impl Scope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&mut self, name: impl Into<String>, value: i64) {
        self.values.insert(name.into(), (value, None));
    }

    /// Binds a constant that was declared with a width.
    pub fn bind_sized(&mut self, name: impl Into<String>, value: i64, width: u32) {
        self.values.insert(name.into(), (value, Some(width)));
    }

    pub fn get(&self, name: &str) -> Option<i64> {
        self.values.get(name).map(|(value, _)| *value)
    }

    pub fn width_of(&self, name: &str) -> Option<u32> {
        self.values.get(name).and_then(|(_, width)| *width)
    }

    /// A copy of this scope, for entering a nested generate block whose genvar
    /// binding must not leak back out.
    pub fn child(&self) -> Self {
        self.clone()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, i64)> {
        self.values.iter().map(|(k, (v, _))| (k.as_str(), *v))
    }

    /// Every binding with its declared width, for copying into another scope.
    pub fn iter_sized(&self) -> impl Iterator<Item = (&str, i64, Option<u32>)> {
        self.values.iter().map(|(k, (v, w))| (k.as_str(), *v, *w))
    }

    /// Takes in every binding of another scope, on top of this one's.
    ///
    /// For a function copied out of a package: its body names the package's
    /// constants bare, and those win over the caller's names of the same
    /// spelling, the way they do inside the package.
    pub fn absorb(&mut self, other: &Scope) {
        for (name, binding) in &other.values {
            self.values.insert(name.clone(), *binding);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    /// A name that is not a constant in this scope — a signal, or a parameter
    /// that was never given a value.
    UnknownName {
        name: String,
        span: Span,
    },
    /// Syntax the front end could not translate, reaching evaluation intact.
    Unsupported {
        text: String,
        span: Span,
    },
    DivideByZero {
        span: Span,
    },
    /// A literal or intermediate too wide for `i64`.
    OutOfRange {
        span: Span,
    },
    /// `'0` / `'1`, whose width comes from an assignment context that a
    /// constant expression does not have.
    ContextDependentFill {
        span: Span,
    },
}

impl EvalError {
    pub fn span(&self) -> Span {
        match self {
            EvalError::UnknownName { span, .. }
            | EvalError::Unsupported { span, .. }
            | EvalError::DivideByZero { span }
            | EvalError::OutOfRange { span }
            | EvalError::ContextDependentFill { span } => *span,
        }
    }

    pub fn message(&self) -> String {
        match self {
            EvalError::UnknownName { name, .. } => {
                format!("`{name}` is not a constant here, so this expression cannot be evaluated")
            }
            EvalError::Unsupported { text, .. } => {
                format!("`{text}` is outside the constant-expression subset")
            }
            EvalError::DivideByZero { .. } => "division by zero in a constant expression".into(),
            EvalError::OutOfRange { .. } => "constant expression does not fit in 64 bits".into(),
            EvalError::ContextDependentFill { .. } => {
                "`'0` and `'1` take their width from what they are assigned to, so they cannot \
                 be used as a constant here"
                    .into()
            }
        }
    }
}

pub type EvalResult = Result<i64, EvalError>;

/// Folds an unresolved expression to an integer.
pub fn eval(expr: &UExpr, scope: &Scope) -> EvalResult {
    let span = expr.span;
    match &*expr.kind {
        UExprKind::Int { value } => Ok(*value),
        UExprKind::Sized { value } => {
            value.to_u64().map(|v| v as i64).ok_or(EvalError::OutOfRange { span })
        }
        UExprKind::Ident { name } => {
            scope.get(name).ok_or_else(|| EvalError::UnknownName { name: name.clone(), span })
        }
        UExprKind::Fill { .. } => Err(EvalError::ContextDependentFill { span }),
        UExprKind::Unary { op, operand } => {
            let value = eval(operand, scope)?;
            Ok(match op {
                UnOp::BitNot => !value,
                UnOp::LogNot => i64::from(value == 0),
                UnOp::Neg => value.checked_neg().ok_or(EvalError::OutOfRange { span })?,
                // Reduction operators over an unsized constant have no defined
                // width to reduce, so they are not in the subset.
                UnOp::RedAnd | UnOp::RedOr | UnOp::RedXor => {
                    return Err(EvalError::Unsupported { text: expr.to_string(), span });
                }
            })
        }
        UExprKind::Binary { op, lhs, rhs } => {
            let a = eval(lhs, scope)?;
            let b = eval(rhs, scope)?;
            binary(*op, a, b, span)
        }
        UExprKind::Ternary { cond, then_value, else_value } => {
            if eval(cond, scope)? != 0 {
                eval(then_value, scope)
            } else {
                eval(else_value, scope)
            }
        }
        UExprKind::Clog2 { arg } => Ok(clog2(eval(arg, scope)?)),
        UExprKind::Cast { width, value } => {
            let width = eval(width, scope)?;
            let value = eval(value, scope)?;
            Ok(truncate(value, width, span)?)
        }
        UExprKind::Concat { .. } | UExprKind::Repl { .. } => concatenate(expr, scope),
        UExprKind::Unsupported { text } => Err(EvalError::Unsupported { text: text.clone(), span }),
        // A function call is logic, not a constant: it is inlined where it is
        // used, which is a place this pass never reaches.
        UExprKind::Call { .. } => Err(EvalError::Unsupported { text: expr.to_string(), span }),
        // A select out of something that does fold is arithmetic: `idx[2:0]`
        // where `idx` is a loop counter is a constant, and refusing it would
        // leave the loop it controls unrollable. A select out of a signal
        // fails on the signal, which is the honest answer.
        UExprKind::Index { base, index } => {
            let base = eval(base, scope)?;
            let index = eval(index, scope)?;
            if !(0..63).contains(&index) {
                return Err(EvalError::OutOfRange { span });
            }
            Ok((base >> index) & 1)
        }
        UExprKind::Range { base, msb, lsb } => {
            let base = eval(base, scope)?;
            let msb = eval(msb, scope)?;
            let lsb = eval(lsb, scope)?;
            if lsb < 0 || msb < lsb || msb >= 63 {
                return Err(EvalError::OutOfRange { span });
            }
            Ok((base >> lsb) & mask((msb - lsb) as u32 + 1))
        }
    }
}

fn binary(op: BinOp, a: i64, b: i64, span: Span) -> EvalResult {
    let overflow = || EvalError::OutOfRange { span };
    Ok(match op {
        BinOp::Add => a.checked_add(b).ok_or_else(overflow)?,
        BinOp::Sub => a.checked_sub(b).ok_or_else(overflow)?,
        BinOp::Mul => a.checked_mul(b).ok_or_else(overflow)?,
        BinOp::Div => {
            if b == 0 {
                return Err(EvalError::DivideByZero { span });
            }
            a.checked_div(b).ok_or_else(overflow)?
        }
        BinOp::Mod => {
            if b == 0 {
                return Err(EvalError::DivideByZero { span });
            }
            a.checked_rem(b).ok_or_else(overflow)?
        }
        BinOp::BitAnd => a & b,
        BinOp::BitOr => a | b,
        BinOp::BitXor => a ^ b,
        BinOp::BitXnor => !(a ^ b),
        // A shift count outside 0..64 is not an overflow in Verilog; it shifts
        // everything out.
        BinOp::Shl => shift(a, b, span, |v, n| v.wrapping_shl(n), 0)?,
        BinOp::Shr => shift(a, b, span, |v, n| ((v as u64).wrapping_shr(n)) as i64, 0)?,
        BinOp::AShr => shift(a, b, span, |v, n| v.wrapping_shr(n), if a < 0 { -1 } else { 0 })?,
        BinOp::Eq => i64::from(a == b),
        BinOp::Ne => i64::from(a != b),
        BinOp::Lt => i64::from(a < b),
        BinOp::Le => i64::from(a <= b),
        BinOp::Gt => i64::from(a > b),
        BinOp::Ge => i64::from(a >= b),
        BinOp::LogAnd => i64::from(a != 0 && b != 0),
        BinOp::LogOr => i64::from(a != 0 || b != 0),
    })
}

/// Applies a shift, saturating to `saturated` once the count exceeds the width.
fn shift(
    value: i64,
    count: i64,
    span: Span,
    apply: impl Fn(i64, u32) -> i64,
    saturated: i64,
) -> EvalResult {
    if count < 0 {
        return Err(EvalError::OutOfRange { span });
    }
    if count >= 64 {
        return Ok(saturated);
    }
    Ok(apply(value, count as u32))
}

/// Folds `{a, b, ...}` and `{n{a}}` by laying the parts out end to end.
///
/// Needs each part's width, which is why declared widths are carried in the
/// scope. A part whose width is unknown, or a total wider than an `i64` can
/// hold, is refused rather than guessed at.
fn concatenate(expr: &UExpr, scope: &Scope) -> EvalResult {
    let span = expr.span;
    let mut bits: i64 = 0;
    let mut total: u32 = 0;

    for (value, width) in flatten(expr, scope)? {
        if width == 0 || total + width > 63 {
            return Err(EvalError::OutOfRange { span });
        }
        bits = (bits << width) | (value & mask(width));
        total += width;
    }
    Ok(bits)
}

/// The parts of a concatenation, each as its value and its width.
fn flatten(expr: &UExpr, scope: &Scope) -> Result<Vec<(i64, u32)>, EvalError> {
    let span = expr.span;
    match &*expr.kind {
        UExprKind::Concat { parts } => {
            let mut out = Vec::new();
            for part in parts {
                out.extend(flatten(part, scope)?);
            }
            Ok(out)
        }
        UExprKind::Repl { count, value } => {
            let count = eval(count, scope)?;
            if !(0..=4096).contains(&count) {
                return Err(EvalError::OutOfRange { span });
            }
            let once = flatten(value, scope)?;
            Ok(once.repeat(count as usize))
        }
        _ => {
            let width = width_of(expr, scope)
                .ok_or_else(|| EvalError::Unsupported { text: expr.to_string(), span })?;
            Ok(vec![(eval(expr, scope)?, width)])
        }
    }
}

/// How many bits a constant expression occupies, when that is knowable.
fn width_of(expr: &UExpr, scope: &Scope) -> Option<u32> {
    match &*expr.kind {
        UExprKind::Sized { value } => Some(value.width),
        // An unsized literal is 32 bits, per IEEE 1800.
        UExprKind::Int { .. } | UExprKind::Clog2 { .. } => Some(32),
        UExprKind::Ident { name } => scope.width_of(name).or(Some(32)),
        UExprKind::Cast { width, .. } => eval(width, scope).ok().map(|w| w as u32),
        UExprKind::Concat { parts } => {
            parts.iter().map(|part| width_of(part, scope)).sum::<Option<u32>>()
        }
        UExprKind::Repl { count, value } => {
            let count = eval(count, scope).ok()? as u32;
            Some(count * width_of(value, scope)?)
        }
        // An operator's result is as wide as its widest operand.
        UExprKind::Binary { lhs, rhs, .. } => {
            Some(width_of(lhs, scope)?.max(width_of(rhs, scope)?))
        }
        UExprKind::Unary { operand, .. } => width_of(operand, scope),
        UExprKind::Ternary { then_value, else_value, .. } => {
            Some(width_of(then_value, scope)?.max(width_of(else_value, scope)?))
        }
        _ => None,
    }
}

/// Keeps the low `width` bits, which is what a cast does.
fn truncate(value: i64, width: i64, span: Span) -> EvalResult {
    if width <= 0 || width > 63 {
        return Err(EvalError::OutOfRange { span });
    }
    Ok(value & mask(width as u32))
}

fn mask(width: u32) -> i64 {
    if width >= 63 { i64::MAX } else { (1i64 << width) - 1 }
}

/// `$clog2(n)`: the number of bits needed to index `n` values.
///
/// IEEE 1800 defines `$clog2(0)` and `$clog2(1)` as 0, which is why a
/// depth-1 memory needs no address bits.
fn clog2(value: i64) -> i64 {
    if value <= 1 {
        return 0;
    }
    let n = (value - 1) as u64;
    (64 - n.leading_zeros()) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtlscope_ir::{ConstBits, FileId};

    fn span() -> Span {
        Span::new(FileId(0), 1, 1, 1)
    }

    fn int(v: i64) -> UExpr {
        UExpr::int(v, span())
    }

    fn ident(name: &str) -> UExpr {
        UExpr::ident(name, span())
    }

    fn bin(op: BinOp, lhs: UExpr, rhs: UExpr) -> UExpr {
        UExpr::new(UExprKind::Binary { op, lhs, rhs }, span())
    }

    fn scope() -> Scope {
        let mut scope = Scope::new();
        scope.bind("W", 8);
        scope.bind("DEPTH", 16);
        scope
    }

    #[test]
    fn arithmetic_folds() {
        // The exact shapes that appear in a width: `W-1`, `(1<<AW)-1`.
        let cases: &[(UExpr, i64)] = &[
            (bin(BinOp::Sub, ident("W"), int(1)), 7),
            (bin(BinOp::Add, ident("W"), ident("DEPTH")), 24),
            (bin(BinOp::Mul, int(3), int(4)), 12),
            (bin(BinOp::Div, int(7), int(2)), 3),
            (bin(BinOp::Mod, int(7), int(2)), 1),
            (bin(BinOp::Sub, bin(BinOp::Shl, int(1), int(4)), int(1)), 15),
        ];
        for (expr, expected) in cases {
            assert_eq!(eval(expr, &scope()), Ok(*expected), "{expr}");
        }
    }

    #[test]
    fn comparisons_and_logic_produce_zero_or_one() {
        let cases: &[(UExpr, i64)] = &[
            (bin(BinOp::Lt, int(1), int(2)), 1),
            (bin(BinOp::Ge, int(1), int(2)), 0),
            (bin(BinOp::LogAnd, int(1), int(0)), 0),
            (bin(BinOp::LogOr, int(0), int(3)), 1),
            (bin(BinOp::Eq, ident("W"), int(8)), 1),
        ];
        for (expr, expected) in cases {
            assert_eq!(eval(expr, &scope()), Ok(*expected), "{expr}");
        }
    }

    #[test]
    fn clog2_matches_ieee_1800() {
        // $clog2(0) and $clog2(1) are 0; otherwise ceil(log2(n)).
        for (input, expected) in [(0, 0), (1, 0), (2, 1), (3, 2), (4, 2), (5, 3), (16, 4), (17, 5)]
        {
            let expr = UExpr::new(UExprKind::Clog2 { arg: int(input) }, span());
            assert_eq!(eval(&expr, &Scope::new()), Ok(expected), "$clog2({input})");
        }
    }

    #[test]
    fn a_sized_literal_folds_to_its_value() {
        let expr = UExpr::new(UExprKind::Sized { value: ConstBits::from_u64(8, 0xFF) }, span());
        assert_eq!(eval(&expr, &Scope::new()), Ok(255));
    }

    #[test]
    fn a_ternary_only_evaluates_the_branch_it_takes() {
        // The untaken branch would fail; a lazy ternary is what makes
        // `WIDTH > 0 ? WIDTH-1 : 0` safe to write.
        let expr = UExpr::new(
            UExprKind::Ternary {
                cond: int(0),
                then_value: ident("does_not_exist"),
                else_value: int(42),
            },
            span(),
        );
        assert_eq!(eval(&expr, &Scope::new()), Ok(42));
    }

    #[test]
    fn an_unknown_name_is_an_error_not_a_zero() {
        let result = eval(&ident("nope"), &Scope::new());
        assert!(matches!(result, Err(EvalError::UnknownName { .. })), "{result:?}");
        assert!(result.unwrap_err().message().contains("nope"));
    }

    #[test]
    fn division_by_zero_is_reported() {
        let expr = bin(BinOp::Div, int(1), int(0));
        assert_eq!(eval(&expr, &Scope::new()), Err(EvalError::DivideByZero { span: span() }));
    }

    #[test]
    fn a_fill_literal_has_no_width_of_its_own() {
        let expr = UExpr::new(UExprKind::Fill { ones: false }, span());
        assert!(matches!(eval(&expr, &Scope::new()), Err(EvalError::ContextDependentFill { .. })));
    }

    #[test]
    fn shifting_past_the_width_clears_rather_than_overflowing() {
        assert_eq!(eval(&bin(BinOp::Shl, int(1), int(100)), &Scope::new()), Ok(0));
        assert_eq!(eval(&bin(BinOp::Shr, int(-1), int(100)), &Scope::new()), Ok(0));
        assert_eq!(eval(&bin(BinOp::AShr, int(-1), int(100)), &Scope::new()), Ok(-1));
    }

    #[test]
    fn a_child_scope_does_not_leak_back() {
        let parent = scope();
        let mut child = parent.child();
        child.bind("i", 3);
        assert_eq!(child.get("i"), Some(3));
        assert_eq!(parent.get("i"), None);
        assert_eq!(child.get("W"), Some(8), "outer bindings are still visible");
    }
}
