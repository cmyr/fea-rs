//! the validation pass
//!
//! Before we start compilation, we do a validation pass. This checks for things
//! like the existence of named glyphs, that referenced classes are defined,
//! and that other constraints of the spec are upheld.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    str::FromStr,
};

use fonttools::types::Tag;
use smol_str::SmolStr;

use super::{glyph_range, tables};
use crate::{
    token_tree::{
        typed::{self, AstNode},
        Token,
    },
    Diagnostic, GlyphMap, Kind, Node,
};

pub struct ValidationCtx<'a> {
    pub errors: Vec<Diagnostic>,
    glyph_map: &'a GlyphMap,
    default_lang_systems: HashSet<(SmolStr, SmolStr)>,
    seen_non_default_script: bool,
    lookup_defs: HashMap<SmolStr, Token>,
    // class and position
    glyph_class_defs: HashMap<SmolStr, Token>,
    mark_class_defs: HashSet<SmolStr>,
    mark_class_used: Option<Token>,
    anchor_defs: HashMap<SmolStr, Token>,
    value_record_defs: HashMap<SmolStr, Token>,
}

impl<'a> ValidationCtx<'a> {
    pub(crate) fn new(glyph_map: &'a GlyphMap) -> Self {
        ValidationCtx {
            glyph_map,
            errors: Vec::new(),
            default_lang_systems: Default::default(),
            seen_non_default_script: false,
            glyph_class_defs: Default::default(),
            lookup_defs: Default::default(),
            mark_class_defs: Default::default(),
            mark_class_used: None,
            anchor_defs: Default::default(),
            value_record_defs: Default::default(),
        }
    }

    fn error(&mut self, range: Range<usize>, message: impl Into<String>) {
        self.errors.push(Diagnostic::error(range, message));
    }

    fn warning(&mut self, range: Range<usize>, message: impl Into<String>) {
        self.errors.push(Diagnostic::warning(range, message));
    }

    pub(crate) fn validate_root(&mut self, node: &typed::Root) {
        for item in node.statements() {
            if let Some(language_system) = typed::LanguageSystem::cast(item) {
                self.validate_language_system(&language_system)
            } else if let Some(class_def) = typed::GlyphClassDef::cast(item) {
                self.validate_glyph_class_def(&class_def);
            } else if let Some(mark_def) = typed::MarkClassDef::cast(item) {
                self.validate_mark_class_def(&mark_def);
            } else if let Some(anchor_def) = typed::AnchorDef::cast(item) {
                self.validate_anchor_def(&anchor_def);
            } else if let Some(_include) = typed::Include::cast(item) {
                //TODO: includes, eh? maybe resolved before now?
            } else if let Some(feature) = typed::Feature::cast(item) {
                self.validate_feature(&feature);
            } else if let Some(table) = typed::Table::cast(item) {
                self.validate_table(&table);
            } else if let Some(lookup) = typed::LookupBlock::cast(item) {
                self.validate_lookup_block(&lookup, true);
            } else if let Some(_value_record_def) = typed::ValueRecordDef::cast(item) {
                unimplemented!("valueRecordDef")
            } else if item.kind() == Kind::AnonKw {
                unimplemented!("anon")
            }
        }
    }

    fn validate_language_system(&mut self, node: &typed::LanguageSystem) {
        let script = node.script();
        let lang = node.language();

        if script.text() == "DFLT" && lang.text() == "dflt" && !self.default_lang_systems.is_empty()
        {
            self.error(
                node.range(),
                "'DFLT dftl' must be first languagesystem statement",
            );
            return;
        }
        if script.text() == "DFLT" {
            if self.seen_non_default_script {
                self.error(
                    script.range(),
                    "languagesystem with 'DFLT' script tag must precede non-'DFLT' languagesystems",
                );
                return;
            }
        } else {
            self.seen_non_default_script = true;
        }

        if !self
            .default_lang_systems
            .insert((script.text().clone(), lang.text().clone()))
        {
            self.warning(node.range(), "Duplicate languagesystem definition");
        }
    }

    fn validate_glyph_class_def(&mut self, node: &typed::GlyphClassDef) {
        let name = node.class_name();
        if let Some(_prev) = self
            .glyph_class_defs
            .insert(name.text().to_owned(), name.token().clone())
        {
            self.warning(name.range(), "duplicate glyph class definition");
            //TODO: use previous span to show previous declaration
            //TODO: have help message
        }
        if let Some(literal) = node.class_def() {
            self.validate_glyph_class_literal(&literal, false);
        } else if let Some(alias) = node.class_alias() {
            self.validate_glyph_class_ref(&alias, false);
        } else {
            self.error(node.range(), "unknown parser bug?");
        }
    }

    fn validate_anchor_def(&mut self, node: &typed::AnchorDef) {
        if let Some(_prev) = self
            .anchor_defs
            .insert(node.name().text.clone(), node.name().clone())
        {
            self.warning(node.name().range(), "duplicate anchor name");
        }
    }

    fn validate_mark_class_def(&mut self, node: &typed::MarkClassDef) {
        if let Some(_use_site) = self.mark_class_used.as_ref() {
            self.error(
                node.keyword().range(),
                "all markClass definitions must precede any use of a mark class in the file",
            );
            //TODO: annotate error with site of use
            //TODO: figure out this:
            //
            // "Note: The mark classes used within a single lookup must be
            // disjoint: none may include a glyph which is in another mark class
            // that is used within the same lookup."
        }
        self.mark_class_defs
            .insert(node.mark_class_name().text().clone());
        self.validate_anchor(&node.anchor());
    }

    fn validate_mark_class(&mut self, node: &typed::GlyphClassName) {
        if !self.mark_class_defs.contains(node.text()) {
            self.error(node.range(), "undefined mark class");
        }
    }

    fn validate_table(&mut self, node: &typed::Table) {
        match node {
            typed::Table::Base(table) => self.validate_base(table),
            typed::Table::Gdef(table) => self.validate_gdef(table),
            typed::Table::Head(table) => self.validate_head(table),
            typed::Table::Hhea(table) => self.validate_hhea(table),
            typed::Table::Vhea(table) => self.validate_vhea(table),
            typed::Table::Name(table) => self.validate_name(table),
            typed::Table::Os2(table) => self.validate_os2(table),
            typed::Table::Stat(table) => self.validate_stat(table),
            _ => (),
        }
    }

    fn validate_base(&mut self, _node: &typed::BaseTable) {
        //TODO: same number of records as there are number of baseline tags
    }

    fn validate_hhea(&mut self, _node: &typed::HheaTable) {
        // lgtm
    }

    fn validate_vhea(&mut self, _node: &typed::VheaTable) {
        // lgtm
    }

    fn validate_os2(&mut self, node: &typed::Os2Table) {
        for item in node.statements() {
            match item {
                typed::Os2TableItem::NumberList(item) => match item.keyword().kind {
                    Kind::PanoseKw => {
                        for number in item.values() {
                            match number.parse_unsigned() {
                                None => self.error(number.range(), "expected positive number"),
                                Some(0..=127) => (),
                                Some(_) => {
                                    self.error(number.range(), "expected value in range 0..128")
                                }
                            }
                        }
                    }
                    Kind::UnicodeRangeKw => {
                        for number in item.values() {
                            if !(0..128).contains(&number.parse_signed()) {
                                self.error(
                                    number.range(),
                                    "expected value in unicode character range 0..=127",
                                );
                            }
                        }
                    }
                    Kind::CodePageRangeKw => {
                        for number in item.values() {
                            if super::tables::OS2::bit_for_code_page(number.parse_signed() as u16)
                                .is_none()
                            {
                                self.error(number.range(), "not a valid code page");
                            }
                        }
                    }
                    _ => unreachable!(),
                },
                typed::Os2TableItem::FamilyClass(item) => {
                    let val = item.value();
                    match val.parse() {
                        Ok(_val) => {
                            //FIXME: check if valid, and warn if not? makeotf
                            // does not validate
                        }
                        Err(e) => self.error(val.range(), e),
                    };
                }
                typed::Os2TableItem::Metric(i) => {
                    if matches!(i.keyword().kind, Kind::WinAscentKw | Kind::WinDescentKw) {
                        let val = i.metric();
                        if val.parse().is_negative() {
                            self.error(val.range(), "expected positive number");
                        }
                    }
                }
                typed::Os2TableItem::Number(item) => {
                    let val = item.number();
                    if val.parse_unsigned().is_none() {
                        self.error(val.range(), "expected positive number");
                    }
                }
                typed::Os2TableItem::Vendor(item) => {
                    let val = item.value();
                    if let Err(e) = Tag::from_str(val.as_str().trim_matches('"')) {
                        self.error(val.range(), format!("invalid tag: '{}'", e));
                    }
                }
            }
        }
    }

    fn validate_stat(&mut self, node: &typed::StatTable) {
        let mut seen_fallback_name = false;
        for item in node.statements() {
            match item {
                typed::StatTableItem::ElidedFallbackName(_) => {
                    if seen_fallback_name {
                        self.error(item.range(), "fallback name must only be defined once");
                    }
                    seen_fallback_name = true;
                }
                typed::StatTableItem::AxisValue(axis) => {
                    let mut seen_location_format = None;
                    for item in axis.statements() {
                        if let typed::StatAxisValueItem::Location(loc) = item {
                            let format = match loc.value() {
                                typed::LocationValue::Value(_) => 'a',
                                typed::LocationValue::MinMax { .. } => 'b',
                                typed::LocationValue::Linked { .. } => 'c',
                            };
                            let prev_format = seen_location_format.replace(format);
                            match (prev_format, format) {
                                (Some('a'), 'a') => (),
                                (Some(_), 'a') => self.error(loc.range(), "multiple location statements, but previous statement was not format 'a'"),
                                (Some(_), 'b' | 'c') => self.error(loc.range(),format!("location statement format '{}' must be only statement", format)),
                                _ => (),
                            }
                        }
                    }
                }
                _ => (),
            }
        }
        if !seen_fallback_name {
            self.error(
                node.tag().range(),
                "STAT table must include 'ElidedFallbackName' or 'ElidedFallbackNameID'",
            );
        }
    }

    fn validate_name(&mut self, node: &typed::NameTable) {
        for record in node.statements() {
            let name_id = record.name_id();
            match name_id.parse() {
                Err(e) => self.error(name_id.range(), e),
                Ok(1..=6) => {
                    self.warning(name_id.range(), "ID's 1-6 are reserved and will be ignored")
                }
                _ => (),
            }
            self.validate_name_spec(&record.entry());
        }
    }

    fn validate_name_spec(&mut self, spec: &typed::NameSpec) {
        let mut platform = None;
        if let Some(id) = spec.platform_id() {
            match id.parse() {
                Err(e) => self.error(id.range(), e),
                Ok(n @ 1 | n @ 3) => platform = Some(n),
                Ok(_) => self.error(id.range(), "platform id must be one of '1' or '3'"),
            }
        };

        let platform = platform.unwrap_or(tables::name::WIN_PLATFORM);

        if let Err((range, err)) = validate_name_string_encoding(platform, spec.string()) {
            self.error(range, err);
        }
        if let Some((platspec, language)) = spec.platform_and_language_ids() {
            if let Err(e) = platspec.parse() {
                self.error(platspec.range(), e);
            }
            if let Err(e) = language.parse() {
                self.error(language.range(), e);
            }
        }
    }

    fn validate_gdef(&mut self, node: &typed::GdefTable) {
        for statement in node.statements() {
            match statement {
                typed::GdefTableItem::ClassDef(node) => {
                    if let Some(cls) = node.base_glyphs() {
                        self.validate_glyph_class(&cls, true);
                    }

                    if let Some(cls) = node.ligature_glyphs() {
                        self.validate_glyph_class(&cls, true);
                    }

                    if let Some(cls) = node.mark_glyphs() {
                        self.validate_glyph_class(&cls, true);
                    }

                    if let Some(cls) = node.component_glyphs() {
                        self.validate_glyph_class(&cls, true);
                    }
                }
                typed::GdefTableItem::Attach(node) => {
                    self.validate_glyph_or_class(&node.target());
                    for idx in node.indices() {
                        if idx.parse_unsigned().is_none() {
                            self.error(idx.range(), "contourpoint indexes must be non-negative");
                        }
                    }
                }
                //FIXME: only one rule allowed per glyph; we need
                //to resolve glyphs here in order to track that.
                typed::GdefTableItem::LigatureCaret(node) => {
                    self.validate_glyph_or_class(&node.target());
                    if let typed::LigatureCaretValue::Pos(node) = node.values() {
                        for idx in node.values() {
                            if idx.parse_unsigned().is_none() {
                                self.error(idx.range(), "contourpoint index must be non-negative");
                            }
                        }
                    }
                }
            }
        }
    }

    fn validate_head(&mut self, node: &typed::HeadTable) {
        let mut prev = None;
        for statement in node.statements() {
            if let Some(prev) = prev.replace(statement.range()) {
                self.warning(prev, "FontRevision overwritten by subsequent statement");
            }
            let value = statement.value();
            let (int, fract) = value.text().split_once('.').expect("checked at parse time");
            if int.parse::<i16>().is_err() {
                let start = value.range().start;
                self.error(start..start + int.len(), "value exceeds 16bit limit");
            }
            if fract.len() != 3 {
                let start = value.range().start + int.len();
                self.warning(
                    start..start + fract.len(),
                    "version number should have exactly three decimal places",
                );
                //TODO: richer error, showing suggested input
            }
        }
    }

    // simple: 'include', 'script', 'language', 'subtable', 'lookup', 'lookupflag',
    // rules: 'enumerate', 'enum', 'ignore', 'substitute', 'sub', 'reversesub', 'rsub', 'position', 'pos',
    // decls: 'markClass', GCLASS}
    // special: 'feature', 'parameters', 'featureNames', 'cvParameters', 'sizemenuname'
    fn validate_feature(&mut self, node: &typed::Feature) {
        let tag = node.tag();
        if tag.text() == "size" {
            return self.validate_size_feature(node);
        }
        let _is_aalt = tag.text() == "aalt";
        // - must occur before anything it references

        let _is_ss = tag.text().starts_with("ss")
            && tag.text()[2..]
                .parse::<u8>()
                .map(|val| val > 1 && val <= 20)
                .unwrap_or(false);

        let _is_cv = tag.text().starts_with("cv")
            && tag.text()[2..]
                .parse::<u8>()
                .map(|val| val > 1 && val <= 99)
                .unwrap_or(false);

        for item in node.statements() {
            if item.kind() == Kind::ScriptNode
                || item.kind() == Kind::LanguageNode
                || item.kind() == Kind::SubtableNode
            {
                // lgtm
            } else if let Some(node) = typed::LookupRef::cast(item) {
                if !self.lookup_defs.contains_key(&node.label().text) {
                    self.error(node.label().range(), "lookup is not defined");
                }
            } else if let Some(node) = typed::LookupBlock::cast(item) {
                self.validate_lookup_block(&node, false);
            } else if let Some(node) = typed::LookupFlag::cast(item) {
                self.validate_lookupflag(&node);
            } else if let Some(node) = typed::GsubStatement::cast(item) {
                self.validate_gsub_statement(&node);
            } else if let Some(node) = typed::GposStatement::cast(item) {
                self.validate_gpos_statement(&node);
            } else if let Some(node) = typed::GlyphClassDef::cast(item) {
                self.validate_glyph_class_def(&node);
            } else if let Some(node) = typed::MarkClassDef::cast(item) {
                self.validate_mark_class_def(&node);
            } else {
                self.warning(
                    item.range(),
                    format!("unhandled item '{}' in feature", item.kind()),
                );
            }
        }
    }

    fn validate_size_feature(&mut self, node: &typed::Feature) {
        let mut param = None;
        let mut menu_name_count = 0;
        for item in node.statements() {
            if let Some(node) = typed::Parameters::cast(item) {
                if param.is_some() {
                    self.error(
                        node.range(),
                        "size feature can have only one 'parameters' statement",
                    );
                }
                param = Some(node);
            } else if let Some(node) = typed::SizeMenuName::cast(item) {
                self.validate_name_spec(&node.spec());
                menu_name_count += 1;
            } else if !item.kind().is_trivia() {
                self.error(
                    item.range(),
                    format!("unexpected item in size feature '{}'", item.kind()),
                );
            }
        }

        match param {
            None => self.error(
                node.tag().range(),
                "size feature must include a 'parameters' statement",
            ),
            Some(param) => {
                if param.subfamily().parse_signed() == 0
                    && param.range_start().map(|x| x.parse() as i32).unwrap_or(0) == 0
                    && param.range_end().map(|x| x.parse() as i32).unwrap_or(0) == 0
                    && menu_name_count != 0
                {
                    //TODO: better diagnostics
                    self.error(
                        param.range(),
                        "if subfamily is omitted, there must be no 'sizemenuname' statements",
                    );
                }
            }
        }
    }

    fn validate_lookup_block(&mut self, node: &typed::LookupBlock, top_level: bool) {
        let name = node.label();
        let mut kind = None;
        if let Some(_prev) = self.lookup_defs.insert(name.text.clone(), name.clone()) {
            //TODO: annotate with previous location
            self.warning(name.range(), "layout label already defined");
        }
        for item in node.statements() {
            if item.kind().is_rule() {
                match kind {
                    Some(kind) if kind != item.kind() => self.error(
                        item.range(),
                        format!(
                            "multiple rule types in lookup block (saw '{}' after '{}')",
                            item.kind(),
                            kind
                        ),
                    ),
                    _ => kind = Some(item.kind()),
                }
            }
            if item.kind() == Kind::ScriptNode || item.kind() == Kind::LanguageNode {
                if top_level {
                    self.error(
                        item.range(),
                        "script and language statements not allowed in standalone lookup blocks",
                    );
                }
            } else if item.kind() == Kind::SubtableNode {
                // lgtm
            } else if let Some(node) = typed::LookupRef::cast(item) {
                if !self.lookup_defs.contains_key(&node.label().text) {
                    self.error(node.label().range(), "lookup is not defined");
                }
            } else if let Some(node) = typed::LookupBlock::cast(item) {
                self.error(
                    node.keyword().range(),
                    "lookup blocks cannot contain other blocks",
                );
                //self.validate_lookup_block(&node, false);
            } else if let Some(node) = typed::LookupFlag::cast(item) {
                self.validate_lookupflag(&node);
            } else if let Some(node) = typed::GsubStatement::cast(item) {
                self.validate_gsub_statement(&node);
            } else if let Some(node) = typed::GposStatement::cast(item) {
                self.validate_gpos_statement(&node);
            } else if let Some(node) = typed::GlyphClassDef::cast(item) {
                self.validate_glyph_class_def(&node);
            } else if let Some(node) = typed::MarkClassDef::cast(item) {
                self.validate_mark_class_def(&node);
            } else {
                self.warning(
                    item.range(),
                    format!("unhandled item {} in lookup block", item.kind()),
                );
            }
        }
    }

    fn validate_gpos_statement(&mut self, node: &typed::GposStatement) {
        match node {
            typed::GposStatement::Type1(rule) => {
                self.validate_glyph_or_class(&rule.target());
                self.validate_value_record(&rule.value());
            }
            typed::GposStatement::Type2(rule) => {
                self.validate_glyph_or_class(&rule.first_item());
                self.validate_glyph_or_class(&rule.second_item());
                self.validate_value_record(&rule.first_value());
                if let Some(second) = rule.second_value() {
                    self.validate_value_record(&second);
                }
            }
            typed::GposStatement::Type3(rule) => {
                self.validate_glyph_or_class(&rule.target());
                self.validate_anchor(&rule.entry());
                self.validate_anchor(&rule.exit());
            }
            //FIXME: this should be also checking that all mark classes referenced
            //in this rule are disjoint
            typed::GposStatement::Type4(rule) => {
                self.validate_glyph_or_class(&rule.base());
                for mark in rule.attachments() {
                    self.validate_anchor(&mark.anchor());
                    match mark.mark_class_name() {
                        Some(name) => self.validate_mark_class(&name),
                        None => {
                            self.error(mark.range(), "mark-to-base attachments should not be null")
                        }
                    }
                }
            }
            typed::GposStatement::Type5(rule) => {
                //FIXME: if this is a class each member should have the same
                //number of ligature components? not sure how we check this.
                self.validate_glyph_or_class(&rule.base());
                for component in rule.ligature_components() {
                    for mark in component.attachments() {
                        let anchor = mark.anchor();
                        match mark.mark_class_name() {
                            Some(name) => self.validate_mark_class(&name),
                            None => {
                                if anchor.null().is_none() {
                                    self.error(
                                        anchor.range(),
                                        "non-NULL anchor must specify mark class",
                                    );
                                }
                            }
                        }
                        self.validate_anchor(&anchor);
                    }
                }
            }
            typed::GposStatement::Type6(rule) => {
                self.validate_glyph_or_class(&rule.base());
                for mark in rule.attachments() {
                    self.validate_anchor(&mark.anchor());
                    match mark.mark_class_name() {
                        Some(name) => self.validate_mark_class(&name),
                        None => {
                            self.error(mark.range(), "mark-to-mark attachments should not be null")
                        }
                    }
                }
            }
            _ => self.fallback_validate_rule(node.node().expect("always a node")),
        }
    }

    fn validate_gsub_statement(&mut self, node: &typed::GsubStatement) {
        match node {
            typed::GsubStatement::Type1(rule) => {
                //TODO: ensure equal lengths, other rerquirements
                self.validate_glyph_or_class(&rule.target());
                self.validate_glyph_or_class(&rule.replacement());
            }
            typed::GsubStatement::Type2(rule) => {
                self.validate_glyph(&rule.target());
                let mut count = 0;
                for item in rule.replacement() {
                    self.validate_glyph(&item);
                    count += 1;
                }
                if count < 2 {
                    let range = range_for_iter(rule.replacement()).unwrap_or_else(|| rule.range());
                    self.error(range, "sequence must contain at least two items");
                }
            }
            typed::GsubStatement::Type3(rule) => {
                self.validate_glyph(&rule.target());
                self.validate_glyph_class(&rule.alternates(), false);
            }
            typed::GsubStatement::Type4(rule) => {
                let mut count = 0;
                for item in rule.target() {
                    self.validate_glyph_or_class(&item);
                    count += 1;
                }
                if count < 2 {
                    let range = range_for_iter(rule.target()).unwrap_or_else(|| rule.range());
                    self.error(range, "sequence must contain at least two items");
                }
                self.validate_glyph(&rule.replacement());
            }
            _ => self.fallback_validate_rule(node.node().expect("always a node")),
        }
    }

    /// we don't currently handle all rules, but we at least check glyph names etc
    fn fallback_validate_rule(&mut self, node: &Node) {
        let range = node
            .iter_tokens()
            .filter(|t| !t.kind.is_trivia())
            .find(|t| t.text.len() > 2)
            .map(|t| t.range())
            .unwrap_or_else(|| node.range());
        self.error(range, format!("unimplemented rule type {}", node.kind));
        for item in node.iter_children() {
            if let Some(node) = typed::GlyphOrClass::cast(item) {
                self.validate_glyph_or_class(&node);
            } else if let Some(anchor) = typed::Anchor::cast(item) {
                self.validate_anchor(&anchor);
            }
        }
    }

    fn validate_lookupflag(&mut self, node: &typed::LookupFlag) {
        if let Some(number) = node.number() {
            if number.text().parse::<u16>().is_err() {
                self.error(number.range(), "value must be a positive 16 bit integer");
            }
            return;
        }

        let mut rtl = false;
        let mut ignore_base = false;
        let mut ignore_lig = false;
        let mut ignore_marks = false;
        let mut mark_set = false;
        let mut filter_set = false;

        let mut iter = node.values();
        while let Some(next) = iter.next() {
            match next.kind() {
                Kind::RightToLeftKw if !rtl => rtl = true,
                Kind::IgnoreBaseGlyphsKw if !ignore_base => ignore_base = true,
                Kind::IgnoreLigaturesKw if !ignore_lig => ignore_lig = true,
                Kind::IgnoreMarksKw if !ignore_marks => ignore_marks = true,

                //FIXME: we are not enforcing some requirements here. in particular,
                // The glyph sets of the referenced classes must not overlap, and the MarkAttachmentType statement can reference at most 15 different classes.
                Kind::MarkAttachmentTypeKw if !mark_set => {
                    mark_set = true;
                    match iter.next().and_then(typed::GlyphClass::cast) {
                        Some(node) => self.validate_glyph_class(&node, true),
                        None => self.error(
                            next.range(),
                            "MarkAttachmentType should be followed by glyph class",
                        ),
                    }
                }
                Kind::UseMarkFilteringSetKw if !filter_set => {
                    filter_set = true;
                    match iter.next().and_then(typed::GlyphClass::cast) {
                        Some(node) => self.validate_glyph_class(&node, true),
                        None => self.error(
                            next.range(),
                            "MarkAttachmentType should be followed by glyph class",
                        ),
                    }
                }
                Kind::RightToLeftKw
                | Kind::IgnoreBaseGlyphsKw
                | Kind::IgnoreMarksKw
                | Kind::IgnoreLigaturesKw
                | Kind::MarkAttachmentTypeKw
                | Kind::UseMarkFilteringSetKw => {
                    self.error(next.range(), "duplicate value in lookupflag")
                }

                _ => self.error(next.range(), "invalid lookupflag value"),
            }
        }
    }

    fn validate_glyph_or_class(&mut self, node: &typed::GlyphOrClass) {
        match node {
            typed::GlyphOrClass::Glyph(name) => self.validate_glyph_name(name),
            typed::GlyphOrClass::Cid(cid) => self.validate_cid(cid),
            typed::GlyphOrClass::Class(class) => self.validate_glyph_class_literal(class, false),
            typed::GlyphOrClass::NamedClass(name) => self.validate_glyph_class_ref(name, false),
            typed::GlyphOrClass::Null(_) => (),
        }
    }

    fn validate_glyph(&mut self, node: &typed::Glyph) {
        match node {
            typed::Glyph::Named(name) => self.validate_glyph_name(name),
            typed::Glyph::Cid(cid) => self.validate_cid(cid),
            typed::Glyph::Null(_) => (),
        }
    }

    fn validate_glyph_class(&mut self, node: &typed::GlyphClass, accept_mark_class: bool) {
        match node {
            typed::GlyphClass::Literal(lit) => {
                self.validate_glyph_class_literal(lit, accept_mark_class)
            }
            typed::GlyphClass::Named(name) => {
                self.validate_glyph_class_ref(name, accept_mark_class)
            }
        }
    }

    fn validate_glyph_class_literal(
        &mut self,
        node: &typed::GlyphClassLiteral,
        accept_mark_class: bool,
    ) {
        for item in node.items() {
            if let Some(id) = typed::GlyphName::cast(item) {
                self.validate_glyph_name(&id);
            } else if let Some(id) = typed::Cid::cast(item) {
                self.validate_cid(&id);
            } else if let Some(range) = typed::GlyphRange::cast(item) {
                self.validate_glyph_range(&range);
            } else if let Some(alias) = typed::GlyphClassName::cast(item) {
                self.validate_glyph_class_ref(&alias, accept_mark_class);
                // these two cases indicate existing errors
            } else if !item.kind().is_trivia()
                && item.kind() != Kind::Ident
                && item.kind() != Kind::GlyphNameOrRange
            {
                self.warning(item.range(), format!("unexpected item {}", item.kind()));
            }
        }
    }

    fn validate_glyph_name(&mut self, name: &typed::GlyphName) {
        if self.glyph_map.get(name.text()).is_none() {
            self.error(name.range(), "glyph not in font");
        }
    }

    fn validate_cid(&mut self, cid: &typed::Cid) {
        if self.glyph_map.get(&cid.parse()).is_none() {
            self.error(cid.range(), "CID not in font");
        }
    }

    fn validate_glyph_class_ref(&mut self, node: &typed::GlyphClassName, accept_mark_class: bool) {
        if accept_mark_class && self.mark_class_defs.contains(node.text()) {
            return;
        }
        if !self.glyph_class_defs.contains_key(node.text()) {
            self.error(node.range(), "undefined glyph class");
        }
    }

    fn validate_glyph_range(&mut self, range: &typed::GlyphRange) {
        let start = range.start();
        let end = range.end();

        match (start.kind, end.kind) {
            (Kind::Cid, Kind::Cid) => {
                if let Err(err) = glyph_range::cid(start, end, |cid| {
                    if self.glyph_map.get(&cid).is_none() {
                        // this is techincally allowed, but we error for now
                        self.warning(
                            range.range(),
                            format!("Range member '{}' does not exist in font", cid),
                        );
                    }
                }) {
                    self.error(range.range(), err);
                }
            }
            (Kind::GlyphName, Kind::GlyphName) => {
                if let Err(err) = glyph_range::named(start, end, |name| {
                    if self.glyph_map.get(name).is_none() {
                        self.warning(
                            range.range(),
                            format!("Range member '{}' does not exist in font", name),
                        );
                    }
                }) {
                    self.error(range.range(), err);
                }
            }
            (_, _) => self.error(range.range(), "Invalid types in glyph range"),
        }
    }

    fn validate_value_record(&mut self, node: &typed::ValueRecord) {
        if let Some(name) = node.named() {
            if !self.value_record_defs.contains_key(&name.text) {
                self.error(name.range(), "undefined value record name");
            }
        }
    }

    fn validate_anchor(&mut self, anchor: &typed::Anchor) {
        if let Some(name) = anchor.name() {
            if !self.anchor_defs.contains_key(&name.text) {
                self.error(name.range(), "undefined anchor name");
            }
        }
    }
}

fn range_for_iter<T: AstNode>(mut iter: impl Iterator<Item = T>) -> Option<Range<usize>> {
    let start = iter.next()?.range();
    Some(iter.fold(start, |cur, node| cur.start..node.range().end))
}

fn validate_name_string_encoding(
    platform: u16,
    string: &Token,
) -> Result<(), (Range<usize>, String)> {
    let mut to_scan: &str = string.as_str();
    let token_start = string.range().start;
    let mut cur_off = 0;
    while !to_scan.is_empty() {
        match to_scan.bytes().position(|b| b == b'\\') {
            None => to_scan = "",
            Some(pos) if platform == tables::name::WIN_PLATFORM => {
                let range_start = token_start + cur_off + pos;
                if let Some(val) = to_scan.get(pos + 1..pos + 5) {
                    if let Some(c) = val.chars().find(|c| !c.is_digit(16)) {
                        return Err((
                            range_start..range_start + 5,
                            format!("invalid escape sequence: '{}' is not a hex digit", c),
                        ));
                    }
                } else {
                    return Err((
                        range_start..range_start + 1,
                        "windows escape sequences must be four hex digits long".into(),
                    ));
                }
                cur_off += to_scan[..pos].len();
                to_scan = &to_scan[pos + 5..];
            }
            Some(pos) => {
                let range_start = token_start + cur_off + pos;
                if let Some(val) = to_scan.get(pos + 1..pos + 3) {
                    if let Some(c) = val.chars().find(|c| !c.is_digit(16)) {
                        return Err((
                            range_start..range_start + 5,
                            format!("invalid escape sequence: '{}' is not a hex digit", c),
                        ));
                    }

                    if let Err(e) = u8::from_str_radix(val, 16) {
                        return Err((
                            range_start..range_start + 3,
                            format!("invalid escape sequence '{}'", e),
                        ));
                    }
                } else {
                    return Err((
                        range_start..range_start + 1,
                        "windows escape sequences must be four hex digits long".into(),
                    ));
                }
                cur_off += to_scan[..pos].len();
                to_scan = &to_scan[pos + 3..];
            }
        }
    }
    Ok(())
}
