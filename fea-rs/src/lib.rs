//! Parsing the Adobe OpenType Feature File format.

mod compile;
mod diagnostic;
mod parse;
mod token_tree;
mod types;

pub use compile::validate;
pub use diagnostic::Diagnostic;
pub use parse::grammar::root;
pub use parse::util;
pub use parse::{Kind, Parser, SyntaxError, TokenSet};
pub use token_tree::{typed, AstSink, Node, NodeOrToken};
pub use types::{GlyphMap, GlyphName};
