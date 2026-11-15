//! ORBIT front end: source management, lexer, parser, AST and diagnostics.

pub mod ast;
pub mod diagnostic;
pub mod format;
pub mod lexer;
pub mod parser;
pub mod source;

pub use diagnostic::{Diagnostic, Severity};
pub use parser::parse;
pub use source::{FileId, Location, SourceMap, Span};
