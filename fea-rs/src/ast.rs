use std::{ops::Range, sync::Arc};

use smol_str::SmolStr;

use crate::{
    parse::{SyntaxError, TreeSink},
    Kind,
};

use self::cursor::Cursor;

mod cursor;
mod stack;

#[derive(PartialEq, Eq, Clone, Copy)]
struct SyntaxKind(u16);

#[derive(Debug, PartialEq, Eq, Clone, PartialOrd, Ord, Hash)]
pub struct Node {
    pub kind: Kind,
    // start of this node relative to start of parent node.
    // we can use this to more efficiently move to a given offset
    // TODO: remove if unused
    rel_pos: usize,
    pub text_len: usize,
    children: Arc<[NodeOrToken]>,
}

#[derive(Debug, PartialEq, Eq, Clone, PartialOrd, Ord, Hash)]
pub struct Token {
    pub kind: Kind,
    pub text: SmolStr,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeOrToken {
    Node(Node),
    Token(Token),
}

#[derive(Clone, Debug, Default)]
struct TreeBuilder {
    //TODO: reuse tokens
    //token_cache: HashMap<Arc<Token>>,
    // the kind of the parent, and the index in children of the first child.
    parents: Vec<(Kind, usize)>,
    children: Vec<NodeOrToken>,
}

pub struct AstSink<'a> {
    text: &'a str,
    text_pos: usize,
    builder: TreeBuilder,
    errors: Vec<SyntaxError>,
}

impl TreeSink for AstSink<'_> {
    fn token(&mut self, kind: Kind, len: usize) {
        let token_text = &self.text[self.text_pos..self.text_pos + len];
        self.builder.token(kind, token_text);
        self.text_pos += len;
    }

    fn start_node(&mut self, kind: Kind) {
        self.builder.start_node(kind);
    }

    fn finish_node(&mut self) {
        self.builder.finish_node();
    }

    fn error(&mut self, error: SyntaxError) {
        self.errors.push(error)
    }
}

impl<'a> AstSink<'a> {
    pub fn new(text: &'a str) -> Self {
        AstSink {
            text,
            text_pos: 0,
            builder: TreeBuilder::default(),
            errors: Vec::new(),
        }
    }

    pub fn finish(self) -> (Node, Vec<SyntaxError>) {
        let node = self.builder.finish();
        (node, self.errors)
    }
}

impl Node {
    fn new(kind: Kind, mut children: Vec<NodeOrToken>) -> Self {
        let mut text_len = 0;
        for child in &mut children {
            if let NodeOrToken::Node(n) = child {
                n.rel_pos += text_len;
            }
            text_len += child.text_len();
        }

        Node {
            kind,
            text_len,
            rel_pos: 0,
            children: children.into(),
        }
    }

    pub fn cursor(&self) -> Cursor {
        Cursor::new(self)
    }

    pub fn iter_tokens(&self) -> impl Iterator<Item = &Token> {
        let mut cursor = Cursor::new(self);
        std::iter::from_fn(move || cursor.next_token())
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn text_len(&self) -> usize {
        self.text_len
    }

    pub fn children(&self) -> impl Iterator<Item = &NodeOrToken> {
        self.children.iter()
    }

    #[doc(hidden)]
    pub fn debug_print_structure(&self, include_tokens: bool) {
        let mut cursor = self.cursor();
        while let Some(thing) = cursor.current() {
            match thing {
                NodeOrToken::Node(node) => {
                    let depth = cursor.depth();
                    eprintln!(
                        "{}{} ({}..{})",
                        &crate::util::SPACES[..depth * 2],
                        node.kind,
                        cursor.pos(),
                        cursor.pos() + node.text_len()
                    );
                }
                NodeOrToken::Token(t) if include_tokens => eprint!("{}", t.as_str()),
                _ => (),
            }
            cursor.advance();
        }
    }
}

impl TreeBuilder {
    fn start_node(&mut self, kind: Kind) {
        let len = self.children.len();
        self.parents.push((kind, len));
    }

    fn token(&mut self, kind: Kind, text: impl Into<SmolStr>) {
        let token = Token {
            kind,
            text: text.into(),
        };
        self.push_raw(NodeOrToken::Token(token));
    }

    fn push_raw(&mut self, item: NodeOrToken) {
        self.children.push(item)
    }

    fn finish_node(&mut self) {
        let (kind, first_child) = self.parents.pop().unwrap();
        let node = Node::new(kind, self.children.split_off(first_child));
        self.children.push(NodeOrToken::Node(node));
    }

    fn finish(mut self) -> Node {
        assert_eq!(self.children.len(), 1);
        self.children.pop().unwrap().into_node().unwrap()
    }
}

impl NodeOrToken {
    pub fn is_token(&self) -> bool {
        matches!(self, NodeOrToken::Token(_))
    }

    pub fn token_text(&self) -> Option<&str> {
        self.as_token().map(Token::as_str)
    }

    pub fn text_len(&self) -> usize {
        match self {
            NodeOrToken::Node(n) => n.text_len,
            NodeOrToken::Token(t) => t.text.len(),
        }
    }

    pub fn into_node(self) -> Option<Node> {
        match self {
            NodeOrToken::Node(node) => Some(node),
            NodeOrToken::Token(_) => None,
        }
    }

    pub fn into_token(self) -> Option<Token> {
        match self {
            NodeOrToken::Node(_) => None,
            NodeOrToken::Token(token) => Some(token),
        }
    }

    pub fn as_node(&self) -> Option<&Node> {
        match self {
            NodeOrToken::Node(node) => Some(node),
            NodeOrToken::Token(_) => None,
        }
    }

    pub fn as_token(&self) -> Option<&Token> {
        match self {
            NodeOrToken::Node(_) => None,
            NodeOrToken::Token(token) => Some(token),
        }
    }
}

impl From<Node> for NodeOrToken {
    fn from(src: Node) -> NodeOrToken {
        NodeOrToken::Node(src)
    }
}

impl Token {
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

fn apply_edits(base: &Node, mut edits: Vec<(Range<usize>, Node)>) -> Node {
    edits.sort_unstable_by_key(|(range, _)| range.start);
    edits.reverse();
    let mut builder = TreeBuilder::default();
    let mut cursor = base.cursor();
    apply_edits_recurse(&mut cursor, &mut builder, &mut edits);
    builder.finish()
}

fn apply_edits_recurse(
    cursor: &mut Cursor,
    builder: &mut TreeBuilder,
    edits: &mut Vec<(Range<usize>, Node)>,
) {
    builder.start_node(cursor.parent_kind());
    while let Some(current) = cursor.current() {
        let next_edit_range = match edits.last() {
            None => {
                builder.push_raw(current.clone());
                cursor.step_over();
                continue;
            }
            Some((range, _)) => range.clone(),
        };

        // now either:
        // - the edit *is* this item, in which case we replace it
        // - the edit is *inside* this item, in which case we recurse,
        // - the edit does not touch this item in which case we push this item
        //   and step over.
        let cur_range = cursor.pos()..cursor.pos() + current.text_len();
        match op_for_node(cur_range, next_edit_range) {
            EditOp::Copy => {
                builder.push_raw(current.clone());
                cursor.step_over();
                //continue;
            }
            EditOp::Replace => {
                builder.push_raw(edits.pop().unwrap().1.into());
                cursor.step_over();
            }
            EditOp::Recurse => {
                cursor.descend_current();
                apply_edits_recurse(cursor, builder, edits);
                cursor.ascend();
                // invariant: we have copied or edited all
                // the items in this subtree.
                cursor.step_over();
            }
        }
    }
    builder.finish_node();
}

fn op_for_node(node_range: Range<usize>, edit_range: Range<usize>) -> EditOp {
    assert!(edit_range.start >= node_range.start);
    if node_range == edit_range {
        EditOp::Replace
    } else if edit_range.start > node_range.start && edit_range.end < node_range.end {
        EditOp::Recurse
    } else {
        assert!(
            edit_range.end <= node_range.start || edit_range.start >= node_range.end,
            "{:?} {:?}",
            edit_range,
            node_range
        );
        EditOp::Copy
    }
}

enum EditOp {
    Replace,
    Recurse,
    Copy,
}

#[cfg(test)]
mod tests {
    use crate::{Parser, TokenSet};

    use super::*;
    static SAMPLE_FEA: &str = include_str!("../test-data/mini.fea");

    #[test]
    fn token_iter() {
        let mut sink = AstSink::new(SAMPLE_FEA);
        let mut parser = Parser::new(SAMPLE_FEA, &mut sink);
        crate::root(&mut parser);
        let (root, _errs) = sink.finish();
        let reconstruct = root.iter_tokens().map(Token::as_str).collect::<String>();

        crate::assert_eq_str!(SAMPLE_FEA, reconstruct);
    }

    fn make_node(fea: &str, f: impl FnOnce(&mut Parser)) -> Node {
        let mut sink = AstSink::new(fea);
        let mut parser = Parser::new(fea, &mut sink);
        f(&mut parser);
        let (root, _errs) = sink.finish();
        root
    }

    #[test]
    fn rewrite() {
        let fea = "\
languagesystem DFLT dftl;
feature liga {
    substitute f i by f_i;
    substitute f l by f_l;
} liga;
";
        let expected = "\
languagesystem hihi ohno;
feature liga {
    substitute f i by f_i;
    sub gg by w_p;
} liga;
";

        let mut sink = AstSink::new(fea);
        let mut parser = Parser::new(fea, &mut sink);
        crate::root(&mut parser);
        let (root, _errs) = sink.finish();

        let replace_lang = {
            let fea = "languagesystem hihi ohno;";
            make_node(fea, |p| crate::parse::grammar::language_system(p))
        };
        let replace_sub = {
            let fea = "sub gg by w_p;";
            make_node(fea, |p| {
                crate::parse::grammar::gsub::gsub(p, TokenSet::FEATURE_BODY_ITEM)
            })
        };

        //root.debug_print_structure(true);
        let edits = vec![(0..25, replace_lang), (72..94, replace_sub)];
        let edited = apply_edits(&root, edits);
        let result = edited.iter_tokens().map(|t| t.as_str()).collect::<String>();
        crate::assert_eq_str!(expected, result);
    }
}
