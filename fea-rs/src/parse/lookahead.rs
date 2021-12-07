//! A type that manages the lexer, maintaning a buffer of tokens for lookahead.
//!
//! Parsing FEA requries unbounded lookahead, so we have to use a resizeable
//! buffer.

use std::{collections::VecDeque, ops::Range};

use super::lexer::{Kind, Lexeme};

#[derive(Default)]
pub(crate) struct Lookahead {
    pending: VecDeque<PendingToken>,
    reuse: Vec<PendingToken>,
}

impl Lookahead {
    pub(crate) fn len(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(crate) fn advance(&mut self) {
        // take the next item, move it to the back, where it will be reused.
        //debug_assert!(!self.pending.is_empty());
        if let Some(next) = self.pending.pop_front() {
            self.reuse.push(next);
        }
    }

    pub(crate) fn nth(&self, n: usize) -> Option<&PendingToken> {
        self.pending.get(n)
    }

    /// the position of the first token, including any whitespace
    pub(crate) fn cur_pos(&self) -> usize {
        self.pending.front().map(|p| p.start_pos).unwrap_or(0)
    }

    /// clears any trivia associated with the first token, and returns the trivia lexemes.
    pub(crate) fn take_first_trivia(&mut self) -> impl Iterator<Item = Lexeme> + '_ {
        self.pending
            .front_mut()
            .map(|pending| {
                pending.start_pos += pending.trivia_len;
                pending.trivia_len = 0;
                pending.preceding_trivia.drain(..)
            })
            .into_iter()
            .flatten()
    }

    pub(crate) fn add(&mut self, f: impl FnOnce(&mut PendingToken)) -> Result<(), LexError> {
        let pos = self
            .pending
            .back()
            .map(|t| t.token_range().end)
            .unwrap_or(0);
        let mut new = self.reuse.pop().unwrap_or(PendingToken::EMPTY);
        new.reset();
        new.start_pos = pos;
        f(&mut new);
        let replacement_kind = match new.token.kind {
            Kind::StringUnterminated => Some(Kind::String),
            Kind::HexEmpty => Some(Kind::Hex),
            _ => None,
        };

        replacement_kind.map(|kind| std::mem::replace(&mut new.token.kind, kind));
        let result = match replacement_kind {
            Some(Kind::StringUnterminated) => Err(LexError {
                range: new.token_range().start..new.token_range().start + 1,
                message: "Unterminated string (missing trailing '\"')",
            }),
            Some(Kind::HexEmpty) => Err(LexError {
                range: new.token_range(),
                message: "Missing digits after hexidecimal prefix.".into(),
            }),
            _ => Ok(()),
        };
        self.pending.push_back(new);
        result
    }
}

pub(crate) struct LexError {
    pub(crate) range: Range<usize>,
    pub(crate) message: &'static str,
}

/// A non-trivia token, as well as any trivia preceding that token.
///
/// We don't want to worry about trivia for the purposes of most parsing,
/// but we do need to track it in the tree. To achieve this, we collect trivia
/// and store it attached to the subsequent non-trivia token, and then add it
/// to the tree when that token is consumed.
pub(crate) struct PendingToken {
    pub(crate) preceding_trivia: Vec<Lexeme>,
    // the position of the first token, including trivia
    pub(crate) start_pos: usize,
    // total length of trivia
    pub(crate) trivia_len: usize,
    pub(crate) token: Lexeme,
}

impl PendingToken {
    pub(crate) const EMPTY: PendingToken = PendingToken {
        preceding_trivia: Vec::new(),
        start_pos: 0,
        trivia_len: 0,
        token: Lexeme::EMPTY,
    };

    fn reset(&mut self) {
        self.start_pos = 0;
        self.trivia_len = 0;
        self.preceding_trivia.clear();
        self.token = Lexeme::EMPTY;
    }

    /// range of the token, ignoring trivia
    pub(crate) fn token_range(&self) -> Range<usize> {
        let start = self.start_pos + self.trivia_len;
        start..start + self.token.len
    }
}
