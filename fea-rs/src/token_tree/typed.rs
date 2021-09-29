//! typing for ast nodes. based on rust-analyzer.

use std::ops::Range;

use smol_str::SmolStr;

use crate::{types::InvalidTag, Kind, Node, NodeOrToken};

use super::Token;

pub trait AstNode {
    fn cast(node: &NodeOrToken) -> Option<Self>
    where
        Self: Sized;

    fn range(&self) -> Range<usize>;
}

macro_rules! ast_token {
    ($typ:ident, $kind:expr) => {
        #[derive(Clone, Debug)]
        pub struct $typ {
            inner: Token,
        }

        impl $typ {
            #[allow(unused)]
            pub fn text(&self) -> &SmolStr {
                &self.inner.text
            }
        }

        impl AstNode for $typ {
            fn cast(node: &NodeOrToken) -> Option<Self> {
                if let NodeOrToken::Token(t) = node {
                    if t.kind == $kind {
                        return Some(Self { inner: t.clone() });
                    }
                }
                None
            }

            fn range(&self) -> std::ops::Range<usize> {
                self.inner.range()
            }
        }
    };
}

macro_rules! ast_node {
    ($typ:ident, $kind:expr) => {
        #[derive(Clone, Debug)]
        pub struct $typ {
            inner: Node,
        }

        impl $typ {
            #[allow(unused)]
            pub fn iter(&self) -> impl Iterator<Item = &NodeOrToken> {
                self.inner.iter_children()
            }

            //#[allow(unused)]
            //pub fn node(&self) -> &Node {
            //&self.inner
            //}
        }

        impl AstNode for $typ {
            fn cast(node: &NodeOrToken) -> Option<Self> {
                if let NodeOrToken::Node(inner) = node {
                    if inner.kind == $kind {
                        return Some(Self {
                            inner: inner.clone(),
                        });
                    }
                }
                None
            }

            fn range(&self) -> std::ops::Range<usize> {
                self.inner.range()
            }
        }
    };
}

ast_token!(Cid, Kind::Cid);
ast_token!(GlyphName, Kind::GlyphName);
ast_token!(Tag, Kind::Tag);
ast_token!(GlyphClassName, Kind::NamedGlyphClass);
ast_token!(Number, Kind::Number);
ast_token!(Metric, Kind::Metric);
ast_node!(GlyphRange, Kind::GlyphRange);
ast_node!(GlyphClassDef, Kind::GlyphClassDefNode);
ast_node!(MarkClassDef, Kind::MarkClassNode);
ast_node!(Anchor, Kind::AnchorNode);
ast_node!(AnchorDef, Kind::AnchorDefNode);
ast_node!(GlyphClassLiteral, Kind::GlyphClass);
ast_node!(LanguageSystem, Kind::LanguageSystemNode);
ast_node!(Include, Kind::IncludeNode);
ast_node!(Feature, Kind::FeatureNode);
ast_node!(Script, Kind::ScriptNode);
ast_node!(Language, Kind::LanguageNode);
ast_node!(LookupFlag, Kind::LookupFlagNode);
ast_node!(LookupRef, Kind::LookupRefNode);
ast_node!(LookupBlock, Kind::LookupBlockNode);

ast_node!(Gsub1, Kind::GsubType1);
ast_node!(Gsub2, Kind::GsubType2);
ast_node!(Gsub3, Kind::GsubType3);
ast_node!(Gsub4, Kind::GsubType4);
ast_node!(Gsub5, Kind::GsubType5);
ast_node!(Gsub6, Kind::GsubType6);
ast_node!(Gsub8, Kind::GsubType8);
ast_node!(GsubIgnore, Kind::GsubIgnore);

pub enum GsubStatement {
    Type1(Gsub1),
    Type2(Gsub2),
    Type3(Gsub3),
    Type4(Gsub4),
    Type5(Gsub5),
    Type6(Gsub6),
    Type8(Gsub8),
    Ignore(GsubIgnore),
}

pub enum GlyphOrClass {
    Glyph(GlyphName),
    Cid(Cid),
    NamedClass(GlyphClassName),
    Class(GlyphClassLiteral),
}

pub enum Glyph {
    Named(GlyphName),
    Cid(Cid),
}

pub enum GlyphClass {
    Named(GlyphClassName),
    Literal(GlyphClassLiteral),
}

impl LanguageSystem {
    pub fn script(&self) -> Tag {
        self.inner.iter_children().find_map(Tag::cast).unwrap()
    }

    pub fn language(&self) -> Tag {
        self.inner
            .iter_children()
            .skip_while(|t| t.kind() != Kind::Tag)
            .skip(1)
            .find_map(Tag::cast)
            .unwrap()
    }
}

impl Tag {
    pub fn parse(&self) -> Result<crate::types::Tag, InvalidTag> {
        self.inner.text.parse()
    }
}

impl GlyphClassDef {
    pub fn class_name(&self) -> GlyphClassName {
        self.inner
            .iter_children()
            .find_map(GlyphClassName::cast)
            .unwrap()
    }

    pub fn class_alias(&self) -> Option<GlyphClassName> {
        //TODO: ensure this returns non in presence of named glyph class inside class block
        self.iter()
            .skip_while(|t| t.kind() != Kind::Eq)
            .find_map(GlyphClassName::cast)
    }

    pub fn class_def(&self) -> Option<GlyphClassLiteral> {
        self.inner.iter_children().find_map(GlyphClassLiteral::cast)
    }
}

impl Cid {
    pub fn parse(&self) -> u32 {
        self.inner.text.parse().expect("cid is already validated")
    }
}

impl GlyphRange {
    pub fn start(&self) -> &Token {
        self.iter()
            .find(|i| i.kind() == Kind::Cid || i.kind() == Kind::GlyphName)
            .and_then(NodeOrToken::as_token)
            .unwrap()
    }

    pub fn end(&self) -> &Token {
        self.iter()
            .skip_while(|t| t.kind() != Kind::Hyphen)
            .find(|i| i.kind() == Kind::Cid || i.kind() == Kind::GlyphName)
            .and_then(NodeOrToken::as_token)
            .unwrap()
    }
}

impl MarkClassDef {
    pub fn glyph_class(&self) -> GlyphOrClass {
        self.iter().find_map(GlyphOrClass::cast).expect("validated")
    }

    pub fn anchor(&self) -> Anchor {
        self.iter().find_map(Anchor::cast).unwrap()
    }

    pub fn mark_class_name(&self) -> GlyphClassName {
        self.iter()
            .skip_while(|t| t.kind() != Kind::AnchorNode)
            .find_map(GlyphClassName::cast)
            .unwrap()
    }
}

impl AnchorDef {
    pub fn anchor(&self) -> Anchor {
        self.iter().find_map(Anchor::cast).unwrap()
    }

    pub fn name(&self) -> &Token {
        self.iter()
            .find(|t| t.kind() == Kind::Ident)
            .and_then(NodeOrToken::as_token)
            .expect("pre-validated")
    }
}

impl Anchor {
    pub fn coords(&self) -> Option<(Metric, Metric)> {
        let tokens = self.iter();
        let mut first = None;

        for token in tokens {
            if let Some(metric) = Metric::cast(token) {
                if let Some(prev) = first.take() {
                    return Some((prev, metric));
                } else {
                    first = Some(metric);
                }
            }
        }
        None
    }

    pub fn contourpoint(&self) -> Option<Number> {
        self.iter().find_map(Number::cast)
    }

    pub fn null(&self) -> Option<&Token> {
        self.iter()
            .find(|t| t.kind() == Kind::NullKw)
            .and_then(NodeOrToken::as_token)
    }

    pub fn name(&self) -> Option<&Token> {
        self.iter()
            .find(|t| t.kind() == Kind::Ident)
            .and_then(NodeOrToken::as_token)
    }
}

impl Number {
    pub fn parse(&self) -> i32 {
        self.text().parse().expect("already validated")
    }

    pub fn parse_unsigned(&self) -> Option<u32> {
        self.text().parse().ok()
    }
}

impl Metric {
    pub fn parse(&self) -> i32 {
        self.text().parse().expect("already validated")
    }
}

impl Feature {
    pub fn tag(&self) -> Tag {
        self.iter().find_map(Tag::cast).unwrap()
    }
}

impl Script {
    pub fn tag(&self) -> Tag {
        self.iter().find_map(Tag::cast).unwrap()
    }
}

impl Language {
    pub fn tag(&self) -> Tag {
        self.iter().find_map(Tag::cast).unwrap()
    }

    pub fn include_dflt(&self) -> Option<&Token> {
        self.iter()
            .find(|t| t.kind() == Kind::IncludeDfltKw)
            .and_then(NodeOrToken::as_token)
    }

    pub fn exclude_dflt(&self) -> Option<&Token> {
        self.iter()
            .find(|t| t.kind() == Kind::ExcludeDfltKw)
            .and_then(NodeOrToken::as_token)
    }

    pub fn required(&self) -> Option<&Token> {
        self.iter()
            .find(|t| t.kind() == Kind::RequiredKw)
            .and_then(NodeOrToken::as_token)
    }
}

impl LookupFlag {
    pub fn number(&self) -> Option<Number> {
        self.iter().find_map(Number::cast)
    }
}

impl LookupRef {
    pub fn label(&self) -> &Token {
        self.iter()
            .find(|t| t.kind() == Kind::Ident)
            .and_then(NodeOrToken::as_token)
            .unwrap()
    }

    #[allow(dead_code)]
    pub fn use_extension(&self) -> Option<&Token> {
        self.iter()
            .take_while(|t| t.kind() != Kind::LBrace)
            .find(|t| t.kind() == Kind::UseExtensionKw)
            .and_then(NodeOrToken::as_token)
    }
}

impl Gsub1 {
    pub fn target(&self) -> GlyphOrClass {
        self.iter().find_map(GlyphOrClass::cast).unwrap()
    }

    pub fn replacement(&self) -> GlyphOrClass {
        self.iter()
            .skip_while(|t| t.kind() != Kind::ByKw)
            .find_map(GlyphOrClass::cast)
            .unwrap()
    }
}

impl Gsub2 {
    pub fn target(&self) -> Glyph {
        self.iter().find_map(Glyph::cast).unwrap()
    }

    pub fn replacement(&self) -> impl Iterator<Item = Glyph> + '_ {
        self.iter()
            .skip_while(|t| t.kind() != Kind::ByKw)
            .filter_map(Glyph::cast)
    }
}

impl Gsub3 {
    pub fn target(&self) -> Glyph {
        self.iter().find_map(Glyph::cast).unwrap()
    }

    pub fn alternates(&self) -> GlyphClass {
        self.iter()
            .skip_while(|t| t.kind() != Kind::ByKw)
            .find_map(GlyphClass::cast)
            .unwrap()
    }
}

impl Gsub4 {
    pub fn target(&self) -> impl Iterator<Item = GlyphOrClass> + '_ {
        self.iter()
            .take_while(|t| t.kind() != Kind::ByKw)
            .filter_map(GlyphOrClass::cast)
    }

    pub fn replacement(&self) -> Glyph {
        self.iter()
            .skip_while(|t| t.kind() != Kind::ByKw)
            .find_map(Glyph::cast)
            .unwrap()
    }
}

impl AstNode for GlyphOrClass {
    fn cast(node: &NodeOrToken) -> Option<Self>
    where
        Self: Sized,
    {
        match node.kind() {
            Kind::GlyphName => GlyphName::cast(node).map(Self::Glyph),
            Kind::Cid => Cid::cast(node).map(Self::Cid),
            Kind::GlyphClass => GlyphClassLiteral::cast(node).map(Self::Class),
            Kind::NamedGlyphClass => GlyphClassName::cast(node).map(Self::NamedClass),
            _ => None,
        }
    }

    fn range(&self) -> Range<usize> {
        match self {
            Self::Glyph(item) => item.range(),
            Self::Cid(item) => item.range(),
            Self::NamedClass(item) => item.range(),
            Self::Class(item) => item.range(),
        }
    }
}

impl AstNode for Glyph {
    fn cast(node: &NodeOrToken) -> Option<Self>
    where
        Self: Sized,
    {
        match node.kind() {
            Kind::GlyphName => GlyphName::cast(node).map(Self::Named),
            Kind::Cid => Cid::cast(node).map(Self::Cid),
            _ => None,
        }
    }

    fn range(&self) -> Range<usize> {
        match self {
            Self::Named(item) => item.range(),
            Self::Cid(item) => item.range(),
        }
    }
}

impl AstNode for GlyphClass {
    fn cast(node: &NodeOrToken) -> Option<Self>
    where
        Self: Sized,
    {
        match node.kind() {
            Kind::GlyphClass => GlyphClassLiteral::cast(node).map(Self::Literal),
            Kind::NamedGlyphClass => GlyphClassName::cast(node).map(Self::Named),
            _ => None,
        }
    }

    fn range(&self) -> Range<usize> {
        match self {
            Self::Literal(item) => item.range(),
            Self::Named(item) => item.range(),
        }
    }
}

impl AstNode for GsubStatement {
    fn cast(node: &NodeOrToken) -> Option<Self>
    where
        Self: Sized,
    {
        match node.kind() {
            Kind::GsubType1 => Gsub1::cast(node).map(Self::Type1),
            Kind::GsubType2 => Gsub2::cast(node).map(Self::Type2),
            Kind::GsubType3 => Gsub3::cast(node).map(Self::Type3),
            Kind::GsubType4 => Gsub4::cast(node).map(Self::Type4),
            Kind::GsubType5 => Gsub5::cast(node).map(Self::Type5),
            Kind::GsubType6 => Gsub6::cast(node).map(Self::Type6),
            Kind::GsubType8 => Gsub8::cast(node).map(Self::Type8),
            Kind::GsubIgnore => GsubIgnore::cast(node).map(Self::Ignore),
            _ => None,
        }
    }

    fn range(&self) -> Range<usize> {
        match self {
            Self::Type1(item) => item.range(),
            Self::Type2(item) => item.range(),
            Self::Type3(item) => item.range(),
            Self::Type4(item) => item.range(),
            Self::Type5(item) => item.range(),
            Self::Type6(item) => item.range(),
            Self::Type8(item) => item.range(),
            Self::Ignore(item) => item.range(),
        }
    }
}
