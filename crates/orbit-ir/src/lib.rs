//! ORBIT typed intermediate representation, `orbit-ir/1`.
//!
//! Shared by the compiler (which produces it) and the runtime (which executes
//! it). Contains resolved types with payload validation, the pure expression
//! language with its evaluator, and the workflow document with invariant checks.

pub mod expr;
pub mod types;
pub mod workflow;

pub use expr::{eval, BinOp, EvalError, Expr, ObjectField, Scope, UnOp};
pub use types::{FieldType, Type, ValidationError};
pub use workflow::*;
