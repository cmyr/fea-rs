//! Convert raw tokens into semantic events

use std::fmt::Display;
use std::ops::Range;

use fonttools::types::Tag;

use super::{
    lexer::{Kind as LexemeKind, Lexeme, Lexer, TokenSet},
    lookahead::Lookahead,
    FileId,
};
use crate::token_tree::{Kind, TreeSink};

use crate::diagnostic::Diagnostic;

const LOOKAHEAD_MIN: usize = 4;
const LOOKAHEAD_RELOAD: usize = 32;

/// A parsing context.
///
/// This type wraps a lexer (responsible for generating base tokens) and exposes
/// an API for inspecting, consuming, remapping, and grouping these tokens, as
/// well as recording any encountered errors.
///
/// This type does not implement the parsing *logic*; it is driven by various
/// functions defined in the [`grammar`] module.
pub struct Parser<'a> {
    lexer: Lexer<'a>,
    sink: &'a mut dyn TreeSink,
    text: &'a str,
    buf: Lookahead,
}

/// An error encountered while parsing.
#[derive(Debug, Clone)]
pub struct SyntaxError {
    pub message: String,
    pub range: Range<usize>,
}

/// A tag and its range.
///
/// We often want to associate errors with tag locations, so we handle
/// them specially.
#[derive(Clone, Debug)]
pub struct TagToken {
    pub tag: Tag,
    pub range: Range<usize>,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(text: &'a str, sink: &'a mut dyn TreeSink) -> Self {
        let mut this = Parser {
            lexer: Lexer::new(text),
            sink,
            text,
            buf: Lookahead::default(),
        };

        // preload the buffer; this accumulates any errors
        this.ensure_lookahead(LOOKAHEAD_RELOAD);
        this
    }

    pub(crate) fn nth_range(&self, n: usize) -> Range<usize> {
        assert!(n < LOOKAHEAD_MIN);
        self.buf.nth(n).unwrap().token_range()
    }

    pub(crate) fn nth(&self, n: usize) -> Lexeme {
        assert!(n < LOOKAHEAD_MIN);
        self.buf.nth(n).unwrap().token
    }

    pub(crate) fn start_node(&mut self, kind: Kind) {
        self.sink.start_node(kind);
    }

    pub(crate) fn finish_node(&mut self) {
        self.sink.finish_node(None);
    }

    pub(crate) fn in_node<R>(&mut self, kind: Kind, f: impl FnOnce(&mut Parser) -> R) -> R {
        self.eat_trivia();
        self.start_node(kind);
        let r = f(self);
        self.finish_node();
        r
    }

    pub(crate) fn finish_and_remap_node(&mut self, new_kind: Kind) {
        self.sink.finish_node(Some(new_kind))
    }

    pub(crate) fn nth_raw(&self, n: usize) -> &[u8] {
        let range = self.nth_range(n);
        &self.text.as_bytes()[range]
    }

    pub(crate) fn current_token_text(&self) -> &str {
        &self.text[self.nth_range(0)]
    }

    /// advance `N` lexemes, remapping to `Kind`.
    fn do_bump<const N: usize>(&mut self, kind: Kind) {
        let mut len = 0;
        for _ in 0..N {
            len += self.nth(0).len;
            self.advance();
        }
        self.sink.token(kind, len);
    }

    fn advance(&mut self) {
        self.eat_trivia();
        self.buf.advance();
        if self.buf.len() < LOOKAHEAD_MIN {
            self.ensure_lookahead(LOOKAHEAD_RELOAD)
        }
    }

    fn ensure_lookahead(&mut self, n: usize) {
        let to_add = (n + 1).saturating_sub(self.buf.len());
        for _ in 0..to_add {
            let Parser {
                lexer, buf: buf2, ..
            } = self;
            let result = buf2.add(|pending| {
                pending.token = loop {
                    let token = lexer.next_token();
                    if token.kind.is_trivia() {
                        pending.trivia_len += token.len;
                        pending.preceding_trivia.push(token);
                    } else {
                        break token;
                    }
                };
            });

            if let Err(lex_err) = result {
                self.sink.error(Diagnostic::error(
                    FileId::CURRENT_FILE,
                    lex_err.range,
                    lex_err.message,
                ));
            }
        }
    }

    /// Eat if the current token matches.
    pub(crate) fn eat(&mut self, raw: impl TokenComparable) -> bool {
        if self.matches(0, raw) {
            self.eat_raw();
            return true;
        }
        false
    }

    /// Consumes all tokens until hitting a recovery item.
    pub(crate) fn eat_until(&mut self, recovery: impl TokenComparable) {
        while !self.at_eof() && !self.matches(0, recovery) {
            self.eat_raw();
        }
    }

    /// Consume until first non-matching token
    pub(crate) fn eat_while(&mut self, token: impl TokenComparable) {
        while self.eat(token) {
            continue;
        }
    }

    /// Consume unless token matches.
    pub(crate) fn eat_unless(&mut self, token: impl TokenComparable) {
        if !self.matches(0, token) {
            self.eat_raw();
        }
    }

    /// Eat the next token, regardless of what it is.
    pub(crate) fn eat_raw(&mut self) {
        self.do_bump::<1>(self.nth(0).kind.to_token_kind());
    }

    /// Eat the next token if it matches `expect`, replacing it with `remap`.
    ///
    /// Necessary for handling keywords, which are not known to the lexer.
    pub(crate) fn eat_remap(&mut self, expect: impl TokenComparable, remap: Kind) -> bool {
        if self.matches(0, expect) {
            self.do_bump::<1>(remap);
            return true;
        }
        false
    }

    pub(crate) fn at_eof(&self) -> bool {
        self.nth(0).kind == LexemeKind::Eof
    }

    /// Eat any trivia preceding the current token.
    ///
    /// This is normally not necessary, because trivia is consumed when eating
    /// other tokens. It is useful only when trivia should or should not be
    /// associated with a particular node.
    pub(crate) fn eat_trivia(&mut self) {
        for token in self.buf.take_first_trivia() {
            self.sink.token(token.kind.to_token_kind(), token.len);
        }
    }

    pub(crate) fn err_and_bump(&mut self, error: impl Into<String>) {
        self.err(error);
        self.eat_raw();
    }

    pub(crate) fn raw_error(&mut self, range: Range<usize>, message: impl Into<String>) {
        self.sink
            .error(Diagnostic::error(FileId::CURRENT_FILE, range, message));
    }

    /// Error, and advance unless the current token matches a predicate.
    pub(crate) fn err_recover(
        &mut self,
        error: impl Into<String>,
        predicate: impl TokenComparable,
    ) {
        self.err(error);
        if !self.matches(0, predicate) {
            self.eat_raw();
        }
    }

    /// write an error, do not advance
    pub(crate) fn err(&mut self, error: impl Into<String>) {
        let err = Diagnostic::error(FileId::CURRENT_FILE, self.nth_range(0), error);
        self.sink.error(err);
    }

    /// Write an error associated *before* the whitespace of the current token.
    ///
    /// In practice this is useful when missing things like semis or braces.
    pub(crate) fn err_before_ws(&mut self, error: impl Into<String>) {
        let pos = self.buf.cur_pos();
        self.raw_error(pos..pos + 1, error);
    }

    /// Write a *warning* before the whitespace of the associated token.
    ///
    /// This only exists so we can warn if a semi is missing after an include
    /// statement (which is common in the wild)
    pub(crate) fn warn_before_ws(&mut self, error: impl Into<String>) {
        let pos = self.buf.cur_pos();
        let diagnostic = Diagnostic::warning(FileId::CURRENT_FILE, pos..pos + 1, error);
        self.sink.error(diagnostic);
    }

    /// consume if the token matches, otherwise error without advancing
    pub(crate) fn expect(&mut self, kind: impl TokenComparable) -> bool {
        if self.eat(kind) {
            return true;
        }
        self.err(format!("Expected {}, found {}", kind, self.nth(0).kind));
        false
    }

    /// semi gets special handling, we don't care to print whatever else we find,
    /// and we want to include whitespace in the range (i.e, it hugs the previous line).
    pub(crate) fn expect_semi(&mut self) -> bool {
        if !self.eat(LexemeKind::Semi) {
            self.err_before_ws("Expected ';'");
            return false;
        }
        true
    }

    pub(crate) fn expect_recover(
        &mut self,
        kind: impl TokenComparable,
        recover: impl TokenComparable,
    ) -> bool {
        if self.eat(kind) {
            return true;
        }
        self.err(format!("Expected {} found {}", kind, self.nth(0).kind));
        if !self.matches(0, recover) {
            self.eat_raw();
        }
        false
    }

    pub(crate) fn expect_remap_recover(
        &mut self,
        expect: impl TokenComparable,
        remap: Kind,
        recover: impl TokenComparable,
    ) -> bool {
        if self.eat_remap(expect, remap) {
            return true;
        }
        self.err(format!("Expected {} found {}", remap, self.nth(0).kind));
        if !self.matches(0, recover) {
            self.eat_raw();
        }
        false
    }

    pub(crate) fn eat_tag(&mut self) -> Option<TagToken> {
        if self.matches(0, TokenSet::TAG_LIKE) {
            if let Ok(tag) = Tag::from_raw(self.nth_raw(0)) {
                let range = self.nth_range(0);
                self.do_bump::<1>(Kind::Tag);
                return Some(TagToken { tag, range });
            }
        }
        None
    }

    pub(crate) fn expect_tag(&mut self, recover: impl TokenComparable) -> Option<TagToken> {
        if self.matches(0, TokenSet::TAG_LIKE) {
            match self.eat_tag() {
                Some(tag) => return Some(tag),
                None => self.err_and_bump("invalid tag"),
            }
        } else {
            self.err_recover(format!("expected tag, found {}", self.nth(0).kind), recover);
        }
        None
    }

    pub(crate) fn matches(&self, nth: usize, token: impl TokenComparable) -> bool {
        token.matches(self.nth(nth).kind)
    }

    pub(crate) fn raw_range(&self, range: Range<usize>) -> &[u8] {
        &self.text.as_bytes()[range]
    }
}

pub(crate) trait TokenComparable: Copy + Display {
    fn matches(&self, kind: LexemeKind) -> bool;
}

impl TokenComparable for LexemeKind {
    fn matches(&self, kind: LexemeKind) -> bool {
        self == &kind
    }
}

impl TokenComparable for Kind {
    fn matches(&self, kind: LexemeKind) -> bool {
        kind.to_token_kind() == *self
    }
}

impl TokenComparable for TokenSet {
    fn matches(&self, kind: LexemeKind) -> bool {
        self.contains(kind)
    }
}
