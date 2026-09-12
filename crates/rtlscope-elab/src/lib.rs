//! Elaboration: unresolved IR in, elaborated IR out.
//!
//! `rtlscope-sv` hands over a design where widths are expressions, parameters are
//! unbound and `generate` blocks are folded. This crate resolves all of that,
//! and deliberately depends only on `rtlscope-ir` — never on `sv-parser` — so the
//! hard part of the project can be tested without a parser in the loop.

pub mod elaborate;
pub(crate) mod inline;
pub(crate) mod proc;
pub mod validate;
pub mod value;

pub use elaborate::{candidate_tops, elaborate};
pub use validate::validate;
pub use value::{EvalError, Scope, eval};
