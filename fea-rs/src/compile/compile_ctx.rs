use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    convert::TryInto,
    ops::Range,
};

use smol_str::SmolStr;

use fonttools::{
    layout::common::{LookupFlags, ValueRecord},
    tables::GDEF::CaretValue,
    tag,
    types::Tag,
};

use crate::{
    parse::SourceMap,
    token_tree::{
        typed::{self, AstNode},
        Token,
    },
    types::{Anchor, GlyphClass, GlyphId, GlyphOrClass},
    Diagnostic, GlyphMap, Kind, NodeOrToken,
};

use super::output::{Compilation, SizeFeature};
use super::tables::{ScriptRecord, Tables};
use super::{consts, glyph_range};
use super::{
    lookups::{AllLookups, FeatureKey, FilterSetId, LookupId, SomeLookup},
    tables::ClassId,
};

pub struct CompilationCtx<'a> {
    glyph_map: &'a GlyphMap,
    source_map: &'a SourceMap,
    pub errors: Vec<Diagnostic>,
    tables: Tables,
    features: BTreeMap<FeatureKey, Vec<LookupId>>,
    default_lang_systems: HashSet<(Tag, Tag)>,
    lookups: AllLookups,
    lookup_flags: LookupFlags,
    cur_mark_filter_set: Option<FilterSetId>,
    cur_language_systems: HashSet<(Tag, Tag)>,
    cur_feature_name: Option<Tag>,
    script: Option<Tag>,
    glyph_class_defs: HashMap<SmolStr, GlyphClass>,
    mark_classes: HashMap<SmolStr, MarkClass>,
    anchor_defs: HashMap<SmolStr, (Anchor, usize)>,
    mark_attach_class_id: HashMap<GlyphClass, u16>,
    mark_filter_sets: HashMap<GlyphClass, FilterSetId>,
    size: Option<SizeFeature>,
    //mark_attach_used_glyphs: HashMap<GlyphId, u16>,
}

struct MarkClass {
    id: u16,
    members: Vec<(GlyphClass, Anchor)>,
}

impl<'a> CompilationCtx<'a> {
    pub(crate) fn new(glyph_map: &'a GlyphMap, source_map: &'a SourceMap) -> Self {
        CompilationCtx {
            glyph_map,
            source_map,
            errors: Vec::new(),
            tables: Tables::default(),
            default_lang_systems: Default::default(),
            glyph_class_defs: Default::default(),
            lookups: Default::default(),
            features: Default::default(),
            mark_classes: Default::default(),
            anchor_defs: Default::default(),
            lookup_flags: LookupFlags::empty(),
            cur_mark_filter_set: Default::default(),
            cur_language_systems: Default::default(),
            cur_feature_name: None,
            script: None,
            mark_attach_class_id: Default::default(),
            mark_filter_sets: Default::default(),
            size: None,
            //mark_attach_used_glyphs: Default::default(),
        }
    }

    pub(crate) fn compile(&mut self, node: &typed::Root) {
        for item in node.statements() {
            if let Some(language_system) = typed::LanguageSystem::cast(item) {
                self.add_language_system(language_system);
            } else if let Some(class_def) = typed::GlyphClassDef::cast(item) {
                self.define_glyph_class(class_def);
            } else if let Some(mark_def) = typed::MarkClassDef::cast(item) {
                self.define_mark_class(mark_def);
            } else if let Some(anchor_def) = typed::AnchorDef::cast(item) {
                self.define_named_anchor(anchor_def);
            } else if let Some(feature) = typed::Feature::cast(item) {
                self.add_feature(feature);
            } else if let Some(lookup) = typed::LookupBlock::cast(item) {
                self.resolve_lookup_block(lookup);
            } else if item.kind() == Kind::AnonBlockNode {
                // noop
            } else if let Some(table) = typed::Table::cast(item) {
                self.resolve_table(table);

                //TODO: includes, eh? maybe resolved before now?
            } else if !item.kind().is_trivia() {
                let span = match item {
                    NodeOrToken::Token(t) => t.range(),
                    NodeOrToken::Node(node) => {
                        let range = node.range();
                        let end = range.end.min(range.start + 16);
                        range.start..end
                    }
                };
                self.error(span, format!("unhandled top-level item: '{}'", item.kind()));
            }
        }
    }

    pub(crate) fn build(&mut self) -> Result<Compilation, Vec<Diagnostic>> {
        if self.errors.iter().any(Diagnostic::is_error) {
            return Err(self.errors.clone());
        }
        if self.tables.GDEF.is_none() {
            self.infer_glyph_classes();
        }
        Ok(Compilation {
            warnings: self.errors.clone(),
            lookups: self.lookups.clone(),
            features: self.features.clone(),
            tables: self.tables.clone(),
            size: self.size.clone(),
        })
    }

    // if a GDEF table is not explicitly defined, we are supposed to create one:
    // http://adobe-type-tools.github.io/afdko/OpenTypeFeatureFileSpecification.html#4f-markclass
    fn infer_glyph_classes(&mut self) {
        let mut gdef = super::tables::GDEF::default();
        self.lookups.infer_glyph_classes(|glyph, class_id| {
            gdef.glyph_classes.insert(glyph, class_id);
        });
        for glyph in self
            .mark_classes
            .values()
            .flat_map(|class| class.members.iter().map(|(cls, _)| cls.iter()))
            .flatten()
        {
            gdef.glyph_classes.insert(glyph, ClassId::Mark);
        }
        if !gdef.glyph_classes.is_empty() {
            self.tables.GDEF = Some(gdef);
        }
    }

    fn error(&mut self, range: Range<usize>, message: impl Into<String>) {
        let (file, range) = self.source_map.resolve_range(range);
        self.errors.push(Diagnostic::error(file, range, message));
    }

    fn warning(&mut self, range: Range<usize>, message: impl Into<String>) {
        let (file, range) = self.source_map.resolve_range(range);
        self.errors.push(Diagnostic::warning(file, range, message));
    }

    fn add_language_system(&mut self, language_system: typed::LanguageSystem) {
        let script = language_system.script();
        let language = language_system.language();
        self.default_lang_systems
            .insert((script.to_raw(), language.to_raw()));
    }

    fn start_feature(&mut self, feature_name: typed::Tag) {
        assert!(self.cur_language_systems.is_empty());
        if !self.default_lang_systems.is_empty() {
            self.cur_language_systems
                .extend(self.default_lang_systems.iter().cloned());
        } else {
            self.cur_language_systems
                .extend([(consts::SCRIPT_DFLT_TAG, consts::LANG_DFLT_TAG)]);
        };

        assert!(
            !self.lookups.has_current(),
            "no lookup should be active at start of feature"
        );
        self.cur_feature_name = Some(feature_name.to_raw());
        self.lookup_flags = LookupFlags::empty();
        self.cur_mark_filter_set = None;
    }

    fn end_feature(&mut self) {
        if let Some((id, _name)) = self.lookups.finish_current() {
            assert!(
                _name.is_none(),
                "lookup blocks are finished before feature blocks"
            );
            self.add_lookup_to_feature(id, self.cur_feature_name.unwrap());
        }
        self.cur_feature_name = None;
        self.cur_language_systems.clear();
        //self.cur_lookup = None;
        self.lookup_flags = LookupFlags::empty();
        self.cur_mark_filter_set = None;
    }

    fn start_lookup_block(&mut self, name: &Token) {
        if self.cur_feature_name == Some(tag!("aalt")) {
            self.error(name.range(), "no lookups allowed in aalt");
        }

        if let Some((id, _name)) = self.lookups.finish_current() {
            assert!(_name.is_none(), "lookup blocks cannot be nested");
            if let Some(feature) = self.cur_feature_name {
                self.add_lookup_to_feature(id, feature);
            }
        }

        if self.cur_feature_name.is_none() {
            self.lookup_flags = LookupFlags::empty();
            self.cur_mark_filter_set = None;
        }

        self.lookups.start_named(name.text.clone());
    }

    fn end_lookup_block(&mut self) {
        let current = self.lookups.finish_current();
        if let Some(feature) = self.cur_feature_name {
            if let Some((id, _)) = current {
                self.add_lookup_to_feature(id, feature);
            }
        } else {
            self.lookup_flags = LookupFlags::empty();
            self.cur_mark_filter_set = None;
        }
    }

    fn set_language(&mut self, stmt: typed::Language) {
        // not currently handled
        if let Some(token) = stmt.required() {
            self.warning(token.range(), "required is not implemented");
        }
        let language = stmt.tag().to_raw();
        let script = self.script.unwrap_or(consts::SCRIPT_DFLT_TAG);
        self.set_script_language(
            script,
            language,
            stmt.exclude_dflt().is_some(),
            stmt.required().is_some(),
            stmt.range(),
        );
    }

    fn set_script(&mut self, stmt: typed::Script) {
        let script = stmt.tag().to_raw();
        self.script = Some(script);
        self.set_script_language(script, consts::LANG_DFLT_TAG, false, false, stmt.range());
    }

    fn set_script_language(
        &mut self,
        script: Tag,
        language: Tag,
        exclude_dflt: bool,
        _required: bool,
        err_range: Range<usize>,
    ) {
        let feature = match self.cur_feature_name {
            Some(tag @ consts::AALT_TAG | tag @ consts::SIZE_TAG) => {
                self.error(
                    err_range,
                    format!("language/script not allowed in '{}' feature", tag),
                );
                return;
            }
            Some(tag) => tag,
            None => {
                self.error(err_range, "language/script only allowed in feature block");
                return;
            }
        };

        if let Some((id, _name)) = self.lookups.finish_current() {
            self.add_lookup_to_feature(id, feature);
        }

        let dflt_key = FeatureKey::for_feature(feature).language(language);
        let real_key = dflt_key.script(script);

        let wants_dflt = dflt_key.language == "dflt" && !exclude_dflt;

        let lookups = wants_dflt
            .then(|| self.features.get(&dflt_key).cloned())
            .flatten()
            .unwrap_or_default();
        self.features.insert(real_key, lookups);

        self.cur_language_systems.clear();
        self.cur_language_systems
            .extend([(real_key.script, real_key.language)]);
    }

    fn set_lookup_flag(&mut self, node: typed::LookupFlag) {
        if let Some(number) = node.number() {
            self.lookup_flags = LookupFlags::from_bits_truncate(number.parse_unsigned().unwrap());
            return;
        }

        let mut flags = LookupFlags::empty();

        let mut iter = node.values();
        while let Some(next) = iter.next() {
            match next.kind() {
                Kind::RightToLeftKw => flags |= LookupFlags::RIGHT_TO_LEFT,
                Kind::IgnoreBaseGlyphsKw => flags |= LookupFlags::IGNORE_BASE_GLYPHS,
                Kind::IgnoreLigaturesKw => flags |= LookupFlags::IGNORE_LIGATURES,
                Kind::IgnoreMarksKw => flags |= LookupFlags::IGNORE_MARKS,

                //FIXME: we are not enforcing some requirements here. in particular,
                // The glyph sets of the referenced classes must not overlap, and the MarkAttachmentType statement can reference at most 15 different classes.
                // ALSO: this should accept mark classes.
                Kind::MarkAttachmentTypeKw => {
                    let node = iter
                        .next()
                        .and_then(typed::GlyphClass::cast)
                        .expect("validated");
                    let mark_attach_set = self.resolve_mark_attach_class(&node);
                    flags |= LookupFlags::from_bits_truncate(mark_attach_set << 8);
                }
                Kind::UseMarkFilteringSetKw => {
                    let node = iter
                        .next()
                        .and_then(typed::GlyphClass::cast)
                        .expect("validated");
                    let filter_set = self.resolve_mark_filter_set(&node);
                    flags |= LookupFlags::USE_MARK_FILTERING_SET;
                    self.cur_mark_filter_set = Some(filter_set);
                }
                other => unreachable!("mark statements have been validated: '{:?}'", other),
            }
        }
        self.lookup_flags = flags;
    }

    fn resolve_mark_attach_class(&mut self, glyphs: &typed::GlyphClass) -> u16 {
        let glyphs = self.resolve_glyph_class(glyphs);
        let mark_set = glyphs.sort_and_dedupe();
        if let Some(id) = self.mark_attach_class_id.get(&mark_set) {
            return *id;
        }

        let id = self.mark_attach_class_id.len() as u16 + 1;
        //FIXME: I don't understand what is not allowed here

        self.mark_attach_class_id.insert(mark_set, id);
        id
    }

    fn resolve_mark_filter_set(&mut self, glyphs: &typed::GlyphClass) -> u16 {
        let glyphs = self.resolve_glyph_class(glyphs);
        let set = glyphs.sort_and_dedupe();
        let id = self.mark_filter_sets.len() + 1;
        *self
            .mark_filter_sets
            .entry(set)
            .or_insert_with(|| id.try_into().unwrap())
    }

    pub fn add_subtable_break(&mut self) {
        if !self.lookups.add_subtable_break() {
            //TODO: report that we weren't in a lookup?
        }
    }

    fn ensure_current_lookup_type(&mut self, kind: Kind) -> &mut SomeLookup {
        if self.lookups.needs_new_lookup(kind) {
            //FIXME: find another way of ensuring that named lookup blocks don't
            //contain mismatched rules
            //assert!(!self.lookups.is_named(), "ensure rule type in validation");
            if let Some(lookup) =
                self.lookups
                    .start_lookup(kind, self.lookup_flags, self.cur_mark_filter_set)
            {
                self.add_lookup_to_feature(lookup, self.cur_feature_name.unwrap());
            }
        }
        self.lookups.current_mut().expect("we just created it")
    }

    fn add_lookup_to_feature(&mut self, lookup: LookupId, feature: Tag) {
        if lookup == LookupId::Empty {
            return;
        }
        let key = FeatureKey::for_feature(feature);
        for (script, lang) in &self.cur_language_systems {
            let key = key.script(*script).language(*lang);
            self.features.entry(key).or_default().push(lookup);
        }
    }

    fn add_gpos_statement(&mut self, node: typed::GposStatement) {
        match node {
            typed::GposStatement::Type1(rule) => self.add_single_pos(&rule),
            typed::GposStatement::Type2(rule) => self.add_pair_pos(&rule),
            typed::GposStatement::Type3(rule) => self.add_cursive_pos(&rule),
            typed::GposStatement::Type4(rule) => self.add_mark_to_base(&rule),
            typed::GposStatement::Type5(rule) => self.add_mark_to_lig(&rule),
            typed::GposStatement::Type6(rule) => self.add_mark_to_mark(&rule),
            typed::GposStatement::Type8(rule) => self.add_contextual_pos_rule(&rule),
            typed::GposStatement::Ignore(rule) => self.add_contextual_pos_ignore(&rule),
        }
    }

    fn add_gsub_statement(&mut self, node: typed::GsubStatement) {
        match node {
            typed::GsubStatement::Type1(rule) => self.add_single_sub(&rule),
            typed::GsubStatement::Type2(rule) => self.add_multiple_sub(&rule),
            typed::GsubStatement::Type3(rule) => self.add_alternate_sub(&rule),
            typed::GsubStatement::Type4(rule) => self.add_ligature_sub(&rule),
            typed::GsubStatement::Type6(rule) => self.add_contextual_sub(&rule),
            typed::GsubStatement::Ignore(rule) => self.add_contextual_sub_ignore(&rule),
            typed::GsubStatement::Type8(rule) => self.add_reverse_contextual_sub(&rule),
            _ => self.warning(node.range(), "unimplemented rule type"),
        }
    }

    fn add_single_sub(&mut self, node: &typed::Gsub1) {
        let target = node.target();
        let replace = node.replacement();

        if let Some((target, replacement)) =
            self.validate_single_sub_inputs(&target, Some(&replace))
        {
            let lookup = self.ensure_current_lookup_type(Kind::GsubType1);
            for (target, replacement) in target.iter().zip(replacement.into_iter_for_target()) {
                lookup.add_gsub_type_1(target, replacement);
            }
        }
    }

    //TODO: this should be in validate, but we don't have access to resolved
    //glyphs there right now :(
    fn validate_single_sub_inputs(
        &mut self,
        target: &typed::GlyphOrClass,
        replace: Option<&typed::GlyphOrClass>,
    ) -> Option<(GlyphOrClass, GlyphOrClass)> {
        let target_ids = self.resolve_glyph_or_class(target);
        let replace_ids = replace
            .map(|r| self.resolve_glyph_or_class(r))
            .unwrap_or(GlyphOrClass::Null);
        match (target_ids, replace_ids) {
            (GlyphOrClass::Null, _) => {
                self.error(target.range(), "NULL is not a valid substitution target");
                None
            }
            (GlyphOrClass::Glyph(_), GlyphOrClass::Class(_)) => {
                self.error(replace.unwrap().range(), "cannot sub glyph by glyph class");
                None
            }
            (GlyphOrClass::Class(c1), GlyphOrClass::Class(c2)) if c1.len() != c2.len() => {
                self.error(
                    replace.unwrap().range(),
                    format!(
                        "class has different length ({}) than target ({})",
                        c1.len(),
                        c2.len()
                    ),
                );
                None
            }
            other => Some(other),
        }
    }

    fn add_multiple_sub(&mut self, node: &typed::Gsub2) {
        let target = node.target();
        let target_id = self.resolve_glyph(&target);
        let replacement = node
            .replacement()
            .map(|g| self.resolve_glyph(&g).to_raw())
            .collect();
        let lookup = self.ensure_current_lookup_type(Kind::GsubType2);
        lookup.add_gsub_type_2(target_id, replacement);
    }

    fn add_alternate_sub(&mut self, node: &typed::Gsub3) {
        let target = self.resolve_glyph(&node.target());
        let alts = self.resolve_glyph_class(&node.alternates());
        let lookup = self.ensure_current_lookup_type(Kind::GsubType3);
        lookup.add_gsub_type_3(target, alts.iter().map(|g| g.to_raw()).collect());
    }

    fn add_ligature_sub(&mut self, node: &typed::Gsub4) {
        let target = node
            .target()
            .map(|g| self.resolve_glyph_or_class(&g))
            .collect::<Vec<_>>();
        let replacement = self.resolve_glyph(&node.replacement());
        let lookup = self.ensure_current_lookup_type(Kind::GsubType4);

        for target in sequence_enumerator(&target) {
            lookup.add_gsub_type_4(target, replacement);
        }
    }

    fn add_contextual_sub(&mut self, node: &typed::Gsub6) {
        let mut backtrack = node
            .backtrack()
            .items()
            .map(|g| make_ctx_glyphs(&self.resolve_glyph_or_class(&g)))
            .collect::<Vec<_>>();
        backtrack.reverse();
        let lookahead = node
            .lookahead()
            .items()
            .map(|g| make_ctx_glyphs(&self.resolve_glyph_or_class(&g)))
            .collect::<Vec<_>>();
        // does this have an inline rule?
        let mut inline = node.inline_rule().and_then(|rule| {
            let input = node.input();
            if input.items().nth(1).is_some() {
                // more than one input: this is a ligature rule
                let target = input
                    .items()
                    .map(|inp| self.resolve_glyph_or_class(&inp.target()))
                    .collect::<Vec<_>>();
                let replacement = self.resolve_glyph(&rule.replacement_glyphs().next().unwrap());
                let lookup = self.ensure_current_lookup_type(Kind::GsubType6);
                //FIXME: we should check that the whole sequence is not present the
                //lookup before adding..
                let mut to_return = None;
                for target in sequence_enumerator(&target) {
                    to_return = Some(
                        lookup
                            .as_gsub_type_6()
                            .add_anon_gsub_type_4(target, replacement),
                    );
                }
                to_return
            } else {
                let target = input.items().next().unwrap().target();
                let replacement = rule.replacements().next().unwrap();
                if let Some((target, replacement)) =
                    self.validate_single_sub_inputs(&target, Some(&replacement))
                {
                    let lookup = self.ensure_current_lookup_type(Kind::GsubType6);
                    Some(
                        lookup
                            .as_gsub_type_6()
                            .add_anon_gsub_type_1(target, replacement),
                    )
                } else {
                    None
                }
            }
        });

        let context = node
            .input()
            .items()
            .map(|item| {
                let glyphs = make_ctx_glyphs(&self.resolve_glyph_or_class(&item.target()));
                let mut lookups = Vec::new();
                // if there's an inline rule it always belongs to the first marked
                // glyph, so this should work? it may need to change for fancier
                // inline rules in the future.
                if let Some(inline) = inline.take() {
                    lookups.push(inline.to_u16_or_die());
                }

                for lookup in item.lookups() {
                    let lookup = self.lookups.get_named(&lookup.label().text).unwrap(); // validated already
                    lookups.push(lookup.to_u16_or_die());
                }
                (glyphs, lookups)
            })
            .collect::<Vec<_>>();

        let lookup = self.ensure_current_lookup_type(Kind::GsubType6);
        lookup.add_gsub_type_6(backtrack, context, lookahead);
    }

    fn add_contextual_sub_ignore(&mut self, node: &typed::GsubIgnore) {
        for rule in node.rules() {
            let mut backtrack = rule
                .backtrack()
                .items()
                .map(|g| make_ctx_glyphs(&self.resolve_glyph_or_class(&g)))
                .collect::<Vec<_>>();
            backtrack.reverse();
            let lookahead = rule
                .lookahead()
                .items()
                .map(|g| make_ctx_glyphs(&self.resolve_glyph_or_class(&g)))
                .collect::<Vec<_>>();
            let context = rule
                .input()
                .items()
                .map(|item| {
                    (
                        make_ctx_glyphs(&self.resolve_glyph_or_class(&item.target())),
                        Vec::new(),
                    )
                })
                .collect::<Vec<_>>();
            let lookup = self.ensure_current_lookup_type(Kind::GsubType6);
            lookup.add_gsub_type_6(backtrack, context, lookahead);
        }
    }

    fn add_reverse_contextual_sub(&mut self, node: &typed::Gsub8) {
        let mut backtrack = node
            .backtrack()
            .items()
            .map(|g| make_ctx_glyphs(&self.resolve_glyph_or_class(&g)))
            .collect::<Vec<_>>();
        backtrack.reverse();
        let lookahead = node
            .lookahead()
            .items()
            .map(|g| make_ctx_glyphs(&self.resolve_glyph_or_class(&g)))
            .collect::<Vec<_>>();

        let input = node.input().items().next().unwrap();
        let target = input.target();
        let replacement = node.inline_rule().and_then(|r| r.replacements().next());
        //FIXME: warn if there are actual lookups here, we don't support that
        if let Some((target, replacement)) =
            self.validate_single_sub_inputs(&target, replacement.as_ref())
        {
            let context = target
                .iter()
                .zip(replacement.into_iter_for_target())
                .map(|(a, b)| (a.to_raw(), b.to_raw()))
                .collect();
            self.ensure_current_lookup_type(Kind::GsubType8)
                .add_gsub_type_8(backtrack, context, lookahead);
        }
    }

    fn add_single_pos(&mut self, node: &typed::Gpos1) {
        let ids = self.resolve_glyph_or_class(&node.target());
        let record = self.resolve_value_record(&node.value());
        let lookup = self.ensure_current_lookup_type(Kind::GposType1);
        for id in ids.iter() {
            lookup.add_gpos_type_1(id, record.clone());
        }
    }

    fn add_pair_pos(&mut self, node: &typed::Gpos2) {
        let first_ids = self.resolve_glyph_or_class(&node.first_item());
        let second_ids = self.resolve_glyph_or_class(&node.second_item());
        let first_value = self.resolve_value_record(&node.first_value());
        let second_value = node
            .second_value()
            .map(|val| self.resolve_value_record(&val))
            .unwrap_or_default();
        let lookup = self.ensure_current_lookup_type(Kind::GposType2);
        for first in first_ids.iter() {
            for second in second_ids.iter() {
                lookup.add_gpos_type_2_specific(
                    first,
                    second,
                    first_value.clone(),
                    second_value.clone(),
                );
            }
        }
    }

    fn add_cursive_pos(&mut self, node: &typed::Gpos3) {
        let ids = self.resolve_glyph_or_class(&node.target());
        // if null it means we've already reported an error and compilation
        // will fail.
        let entry = self.resolve_anchor(&node.entry()).unwrap_or(Anchor::Null);
        let exit = self.resolve_anchor(&node.exit()).unwrap_or(Anchor::Null);
        let lookup = self.ensure_current_lookup_type(Kind::GposType3);
        for id in ids.iter() {
            lookup.add_gpos_type_3(id, entry, exit)
        }
    }

    fn add_mark_to_base(&mut self, node: &typed::Gpos4) {
        let base_ids = self.resolve_glyph_or_class(&node.base());
        let _ = self.ensure_current_lookup_type(Kind::GposType4);
        for mark in node.attachments() {
            let base_anchor = self.resolve_anchor(&mark.anchor()).unwrap_or(Anchor::Null);
            // ensure we're in the right lookup but drop the reference

            let mark_class_node = mark.mark_class_name().expect("checked in validation");
            let mark_class = self.mark_classes.get(mark_class_node.text()).unwrap();

            // access the lookup through the field, so the borrow checker
            // doesn't think we're borrowing all of self
            //TODO: we do validation here because our validation pass isn't smart
            //enough. We need to not just validate a rule, but every rule in a lookup.
            let maybe_bad_mark = self
                .lookups
                .current_mut()
                .unwrap()
                .with_gpos_type_4(|subtable| {
                    for (glyphs, mark_anchor) in &mark_class.members {
                        let anchor = mark_anchor
                            .to_raw()
                            .expect("no null anchors in mark-to-base (check validation)");
                        for glyph in glyphs.iter() {
                            // validate here that classes are disjoint
                            let prev = subtable
                                .marks
                                .insert(glyph.to_raw(), (mark_class.id, anchor));
                            if let Some(id) =
                                prev.map(|(id, _)| id).filter(|id| *id != mark_class.id)
                            {
                                return Err(id);
                            }
                        }
                    }
                    for base in base_ids.iter() {
                        subtable.bases.entry(base.to_raw()).or_default().insert(
                            mark_class.id,
                            base_anchor
                                .to_raw()
                                .expect("no null anchors in mark-to-base"),
                        );
                    }
                    Ok(())
                });
            if let Err(mark_id) = maybe_bad_mark {
                let prev_class_name = self
                    .mark_classes
                    .iter()
                    .find_map(|(name, class)| (class.id == mark_id).then(|| name.clone()))
                    .unwrap();
                self.error(
                    mark_class_node.range(),
                    format!(
                        "mark class includes glyph in class '{}', already used in lookup.",
                        prev_class_name
                    ),
                );
            }
        }
    }

    fn add_mark_to_lig(&mut self, node: &typed::Gpos5) {
        let base_ids = self.resolve_glyph_or_class(&node.base());
        //table
        for component in node.ligature_components() {
            let lookup = self.ensure_current_lookup_type(Kind::GposType5);
            // add an empty map for the component, to be used below
            lookup.with_gpos_type_5(|subtable| {
                for base in base_ids.iter() {
                    subtable
                        .ligatures
                        .entry(base.to_raw())
                        .or_default()
                        .push(Default::default());
                }
            });

            for mark in component.attachments() {
                let base_anchor = self.resolve_anchor(&mark.anchor()).unwrap_or(Anchor::Null);
                // ensure we're in the right lookup but drop the reference

                // this is allowed to be null
                let mark_class_node = match mark.mark_class_name() {
                    Some(node) => node,
                    None => continue,
                };
                let mark_class = self.mark_classes.get(mark_class_node.text()).unwrap();

                // access the lookup through the field, so the borrow checker
                // doesn't think we're borrowing all of self
                //TODO: we do validation here because our validation pass isn't smart
                //enough. We need to not just validate a rule, but every rule in a lookup.
                let maybe_bad_mark =
                    self.lookups
                        .current_mut()
                        .unwrap()
                        .with_gpos_type_5(|subtable| {
                            for (glyphs, mark_anchor) in &mark_class.members {
                                let anchor = mark_anchor
                                    .to_raw()
                                    .expect("no null anchors in mark-to-base (check validation)");
                                for glyph in glyphs.iter() {
                                    // validate here that classes are disjoint
                                    let prev = subtable
                                        .marks
                                        .insert(glyph.to_raw(), (mark_class.id, anchor));
                                    if let Some(id) =
                                        prev.map(|(id, _)| id).filter(|id| *id != mark_class.id)
                                    {
                                        return Err(id);
                                    }
                                }
                            }
                            for base in base_ids.iter() {
                                subtable
                                    .ligatures
                                    .get_mut(&base.to_raw())
                                    .unwrap() // we just created this at top of loop
                                    .last_mut()
                                    .unwrap() // ditto
                                    .insert(
                                        mark_class.id,
                                        base_anchor
                                            .to_raw()
                                            .expect("no null anchors in mark-to-base"),
                                    );
                            }
                            Ok(())
                        });
                if let Err(mark_id) = maybe_bad_mark {
                    let prev_class_name = self
                        .mark_classes
                        .iter()
                        .find_map(|(name, class)| (class.id == mark_id).then(|| name.clone()))
                        .unwrap();
                    self.error(
                        mark_class_node.range(),
                        format!(
                            "mark class includes glyph in class '{}', already used in lookup.",
                            prev_class_name
                        ),
                    );
                }
            }
        }
    }

    //FIXME: this is basically identical to type 4, but the validation stuff
    //makes it all a big PITA. when we have better validation, we can probably improve this
    //significantly.
    fn add_mark_to_mark(&mut self, node: &typed::Gpos6) {
        let base_ids = self.resolve_glyph_or_class(&node.base());
        let _ = self.ensure_current_lookup_type(Kind::GposType6);
        for mark in node.attachments() {
            let base_anchor = self.resolve_anchor(&mark.anchor()).unwrap_or(Anchor::Null);
            // ensure we're in the right lookup but drop the reference

            let mark_class_node = mark.mark_class_name().expect("checked in validation");
            let mark_class = self.mark_classes.get(mark_class_node.text()).unwrap();

            // access the lookup through the field, so the borrow checker
            // doesn't think we're borrowing all of self
            //TODO: we do validation here because our validation pass isn't smart
            //enough. We need to not just validate a rule, but every rule in a lookup.
            let maybe_bad_mark = self
                .lookups
                .current_mut()
                .unwrap()
                .with_gpos_type_6(|subtable| {
                    for (glyphs, mark_anchor) in &mark_class.members {
                        let anchor = mark_anchor
                            .to_raw()
                            .expect("no null anchors in mark-to-base (check validation)");
                        for glyph in glyphs.iter() {
                            // validate here that classes are disjoint
                            let prev = subtable
                                .combining_marks
                                .insert(glyph.to_raw(), (mark_class.id, anchor));
                            if let Some(id) =
                                prev.map(|(id, _)| id).filter(|id| *id != mark_class.id)
                            {
                                return Err(id);
                            }
                        }
                    }
                    for base in base_ids.iter() {
                        subtable
                            .base_marks
                            .entry(base.to_raw())
                            .or_default()
                            .insert(
                                mark_class.id,
                                base_anchor
                                    .to_raw()
                                    .expect("no null anchors in mark-to-base"),
                            );
                    }
                    Ok(())
                });
            if let Err(mark_id) = maybe_bad_mark {
                let prev_class_name = self
                    .mark_classes
                    .iter()
                    .find_map(|(name, class)| (class.id == mark_id).then(|| name.clone()))
                    .unwrap();
                self.error(
                    mark_class_node.range(),
                    format!(
                        "mark class includes glyph in class '{}', already used in lookup.",
                        prev_class_name
                    ),
                );
            }
        }
    }

    fn add_contextual_pos_rule(&mut self, node: &typed::Gpos8) {
        let mut backtrack = node
            .backtrack()
            .items()
            .map(|g| make_ctx_glyphs(&self.resolve_glyph_or_class(&g)))
            .collect::<Vec<_>>();
        backtrack.reverse();
        let lookahead = node
            .lookahead()
            .items()
            .map(|g| make_ctx_glyphs(&self.resolve_glyph_or_class(&g)))
            .collect::<Vec<_>>();

        let context = node
            .input()
            .items()
            .map(|item| {
                let glyphs = self.resolve_glyph_or_class(&item.target());
                let mut lookups = Vec::new();
                if let Some(value) = item.valuerecord() {
                    let value = self.resolve_value_record(&value);
                    let anon_id = self
                        .ensure_current_lookup_type(Kind::GposType8)
                        .as_gpos_type_8()
                        .add_anon_gpos_type_1(&glyphs, value);
                    lookups.push(anon_id.to_u16_or_die());
                }

                for lookup in item.lookups() {
                    let id = self.lookups.get_named(&lookup.label().text).unwrap();
                    lookups.push(id.to_u16_or_die());
                }

                (make_ctx_glyphs(&glyphs), lookups)
            })
            .collect();
        self.ensure_current_lookup_type(Kind::GposType8)
            .add_gpos_type_8(backtrack, context, lookahead);
    }

    fn add_contextual_pos_ignore(&mut self, node: &typed::GposIgnore) {
        for rule in node.rules() {
            let mut backtrack = rule
                .backtrack()
                .items()
                .map(|g| make_ctx_glyphs(&self.resolve_glyph_or_class(&g)))
                .collect::<Vec<_>>();
            backtrack.reverse();
            let lookahead = rule
                .lookahead()
                .items()
                .map(|g| make_ctx_glyphs(&self.resolve_glyph_or_class(&g)))
                .collect::<Vec<_>>();
            let context = rule
                .input()
                .items()
                .map(|item| {
                    (
                        make_ctx_glyphs(&self.resolve_glyph_or_class(&item.target())),
                        Vec::new(),
                    )
                })
                .collect::<Vec<_>>();
            let lookup = self.ensure_current_lookup_type(Kind::GposType8);
            lookup.add_gpos_type_8(backtrack, context, lookahead);
        }
    }

    fn resolve_value_record(&mut self, record: &typed::ValueRecord) -> ValueRecord {
        if let Some(x_adv) = record.advance() {
            //FIXME: whether this is x or y depends on the current feature?
            return ValueRecord {
                xAdvance: Some(x_adv.parse_signed()),
                ..Default::default()
            };
        }
        if let Some([x_place, y_place, x_adv, y_adv]) = record.placement() {
            return ValueRecord {
                xAdvance: Some(x_adv.parse_signed()),
                yAdvance: Some(y_adv.parse_signed()),
                xPlacement: Some(x_place.parse_signed()),
                yPlacement: Some(y_place.parse_signed()),
                ..Default::default()
            };
        }
        if let Some(name) = record.named() {
            //FIXME:
            self.warning(name.range(), "named value records not implemented yet");
        }

        ValueRecord::default()
    }

    fn define_glyph_class(&mut self, class_decl: typed::GlyphClassDef) {
        let name = class_decl.class_name();
        let glyphs = if let Some(class) = class_decl.class_def() {
            self.resolve_glyph_class_literal(&class)
        } else if let Some(alias) = class_decl.class_alias() {
            self.resolve_named_glyph_class(&alias)
        } else {
            panic!("write more code I guess");
        };

        self.glyph_class_defs.insert(name.text().clone(), glyphs);
    }

    fn define_mark_class(&mut self, class_decl: typed::MarkClassDef) {
        let class_items = class_decl.glyph_class();
        let class_items = self.resolve_glyph_or_class(&class_items).into();

        let anchor = self
            .resolve_anchor(&class_decl.anchor())
            .unwrap_or(Anchor::Null);
        let class_name = class_decl.mark_class_name();
        if let Some(class) = self.mark_classes.get_mut(class_name.text()) {
            class.members.push((class_items, anchor));
        } else {
            let class = MarkClass {
                id: self.mark_classes.len().try_into().unwrap(),
                members: vec![(class_items, anchor)],
            };
            self.mark_classes.insert(class_name.text().clone(), class);
        }
    }

    fn add_feature(&mut self, feature: typed::Feature) {
        let tag = feature.tag();
        let tag_raw = tag.to_raw();
        if tag_raw == consts::AALT_TAG {
            self.warning(tag.range(), "aalt feature is unimplemented");
            return;
        }
        self.start_feature(tag);
        if tag_raw == consts::SIZE_TAG {
            self.resolve_size_feature(&feature);
        } else {
            for item in feature.statements() {
                self.resolve_statement(item);
            }
        }
        self.end_feature();
    }

    fn resolve_size_feature(&mut self, feature: &typed::Feature) {
        fn resolve_decipoint(node: &typed::FloatLike) -> i16 {
            match node {
                typed::FloatLike::Number(n) => n.parse_signed(),
                typed::FloatLike::Float(f) => {
                    let f = f.parse();
                    (f * 10.0).round() as i16
                }
            }
        }

        let mut size = SizeFeature::default();
        for statement in feature.statements() {
            if let Some(node) = typed::SizeMenuName::cast(statement) {
                size.names.push(self.resolve_name_spec(&node.spec()));
            } else if let Some(node) = typed::Parameters::cast(statement) {
                size.params.0 = resolve_decipoint(&node.design_size());
                size.params.1 = node.subfamily().parse_signed();
                size.params.2 = node
                    .range_start()
                    .as_ref()
                    .map(resolve_decipoint)
                    .unwrap_or(0);
                size.params.3 = node
                    .range_end()
                    .as_ref()
                    .map(resolve_decipoint)
                    .unwrap_or(0);
            }
        }
        let key = FeatureKey::for_feature(consts::SIZE_TAG);
        for (script, lang) in &self.cur_language_systems {
            let key = key.script(*script).language(*lang);
            self.features.entry(key).or_default();
        }
        self.size = Some(size);
    }

    fn resolve_table(&mut self, table: typed::Table) {
        match table {
            typed::Table::Base(table) => self.resolve_base(&table),
            typed::Table::Hhea(table) => self.resolve_hhea(&table),
            typed::Table::Vhea(table) => self.resolve_vhea(&table),
            typed::Table::Vmtx(table) => self.resolve_vmtx(&table),
            typed::Table::Name(table) => self.resolve_name(&table),
            typed::Table::Gdef(table) => self.resolve_gdef(&table),
            typed::Table::Head(table) => self.resolve_head(&table),
            typed::Table::Os2(table) => self.resolve_os2(&table),
            typed::Table::Stat(table) => self.resolve_stat(&table),
            _ => (),
        }
    }

    fn resolve_base(&mut self, table: &typed::BaseTable) {
        let mut base = super::tables::BASE::default();
        if let Some(list) = table.horiz_base_tag_list() {
            base.horiz_tag_list = list.tags().map(|t| t.to_raw()).collect();
        }
        if let Some(list) = table.horiz_base_script_record_list() {
            base.horiz_script_list = list
                .script_records()
                .map(|record| ScriptRecord {
                    script: record.script().to_raw(),
                    default_baseline_tag: record.default_baseline().to_raw(),
                    values: record.values().map(|i| i.parse_signed()).collect(),
                })
                .collect();
        }

        if let Some(list) = table.vert_base_tag_list() {
            base.vert_tag_list = list.tags().map(|t| t.to_raw()).collect();
        }
        if let Some(list) = table.vert_base_script_record_list() {
            base.vert_script_list = list
                .script_records()
                .map(|record| ScriptRecord {
                    script: record.script().to_raw(),
                    default_baseline_tag: record.default_baseline().to_raw(),
                    values: record.values().map(|i| i.parse_signed()).collect(),
                })
                .collect();
        }
        self.tables.BASE = Some(base);
    }

    fn resolve_name(&mut self, table: &typed::NameTable) {
        let mut name = super::tables::name::default();
        for record in table.statements() {
            let name_id = record.name_id().parse().unwrap();
            let spec = self.resolve_name_spec(&record.entry());
            name.records
                .push(super::tables::NameRecord { spec, name_id })
        }
        self.tables.name = Some(name);
    }

    fn resolve_os2(&mut self, table: &typed::Os2Table) {
        let mut os2 = super::tables::OS2::default();
        for item in table.statements() {
            match item {
                typed::Os2TableItem::Number(val) => {
                    let value = val.number().parse_unsigned().unwrap();
                    match val.keyword().text.as_str() {
                        "WeightClass" => os2.weight_class = value,
                        "WidthClass" => os2.width_class = value,
                        "LowerOpSize" => os2.lower_op_size = Some(value),
                        "UpperOpSize" => os2.upper_op_size = Some(value),
                        "FSType" => os2.fs_type = value,
                        _ => unreachable!("checked at parse time"),
                    }
                }
                typed::Os2TableItem::Metric(val) => {
                    let value = val.metric().parse();
                    match val.keyword().kind {
                        Kind::TypoAscenderKw => os2.typo_ascender = value,
                        Kind::TypoDescenderKw => os2.typo_descender = value,
                        Kind::TypoLineGapKw => os2.typo_line_gap = value,
                        Kind::XHeightKw => os2.x_height = value,
                        Kind::CapHeightKw => os2.cap_height = value,
                        Kind::WinAscentKw => os2.win_ascent = value as u16,
                        Kind::WinDescentKw => os2.win_descent = value as u16,
                        _ => unreachable!("checked at parse time"),
                    }
                }
                typed::Os2TableItem::NumberList(list) => match list.keyword().kind {
                    Kind::PanoseKw => {
                        for (i, val) in list.values().enumerate() {
                            os2.panose[i] = val.parse_signed() as u8;
                        }
                    }
                    Kind::UnicodeRangeKw => {
                        for val in list.values() {
                            os2.unicode_range |= 1 << val.parse_signed() as usize;
                        }
                    }
                    Kind::CodePageRangeKw => {
                        for val in list.values() {
                            let bit =
                                super::tables::OS2::bit_for_code_page(val.parse_signed() as u16)
                                    .unwrap();
                            os2.code_page_range |= 1 << bit as usize;
                        }
                    }
                    _ => unreachable!("checked at parse time"),
                },
                typed::Os2TableItem::Vendor(item) => {
                    os2.vendor_id = item.value().text.trim_matches('"').into()
                }
                typed::Os2TableItem::FamilyClass(item) => {
                    os2.family_class = item.value().parse().unwrap() as i16
                }
            }
        }
        self.tables.OS2 = Some(os2);
    }

    fn resolve_stat(&mut self, table: &typed::StatTable) {
        let mut stat = super::tables::STAT {
            name: super::tables::StatFallbackName::Id(u16::MAX),
            records: Vec::new(),
            values: Vec::new(),
        };

        for item in table.statements() {
            match item {
                typed::StatTableItem::ElidedFallbackName(name) => {
                    if let Some(id) = name.elided_fallback_name_id() {
                        //FIXME: validate
                        stat.name =
                            super::tables::StatFallbackName::Id(id.parse_unsigned().unwrap());
                    } else {
                        let names = name.names().map(|n| self.resolve_name_spec(&n)).collect();
                        stat.name = super::tables::StatFallbackName::Record(names);
                    }
                }
                typed::StatTableItem::AxisValue(value) => {
                    stat.values.push(self.resolve_stat_axis_value(&value));
                }
                typed::StatTableItem::DesignAxis(value) => {
                    let tag = value.tag().to_raw();
                    let ordering = value.ordering().parse_unsigned().unwrap();
                    //FIXME: validate
                    let name = value.names().map(|n| self.resolve_name_spec(&n)).collect();
                    stat.records.push(super::tables::AxisRecord {
                        tag,
                        ordering,
                        name,
                    });
                }
            }
        }
        self.tables.STAT = Some(stat);
    }

    fn resolve_stat_axis_value(&mut self, node: &typed::StatAxisValue) -> super::tables::AxisValue {
        use super::tables::AxisLocation;
        let mut flags = 0;
        let mut name = Vec::new();
        let mut location = None;
        for item in node.statements() {
            match item {
                typed::StatAxisValueItem::Flag(flag) => {
                    for bit in flag.bits() {
                        flags |= bit;
                    }
                }
                typed::StatAxisValueItem::NameRecord(record) => {
                    name.push(self.resolve_name_spec(&record));
                }
                typed::StatAxisValueItem::Location(loc) => {
                    let loc_tag = loc.tag().to_raw();
                    match loc.value() {
                        typed::LocationValue::Value(num) => {
                            let val = num.parse();
                            if let Some(AxisLocation::One { tag, value }) = location.as_ref() {
                                location = Some(AxisLocation::Four(vec![(*tag, *value)]));
                            }
                            if let Some(AxisLocation::Four(vals)) = location.as_mut() {
                                vals.push((loc_tag, val));
                            } else {
                                location = Some(AxisLocation::One {
                                    tag: loc_tag,
                                    value: val,
                                });
                            }
                        }
                        typed::LocationValue::MinMax { nominal, min, max } => {
                            location = Some(AxisLocation::Two {
                                tag: loc_tag,
                                nominal: nominal.parse(),
                                max: max.parse(),
                                min: min.parse(),
                            });
                        }
                        typed::LocationValue::Linked { value, linked } => {
                            location = Some(AxisLocation::Three {
                                tag: loc_tag,
                                value: value.parse(),
                                linked: linked.parse(),
                            });
                        }
                    }
                }
            }
        }

        super::tables::AxisValue {
            flags,
            name,
            location: location.unwrap(),
        }
    }

    fn resolve_hhea(&mut self, table: &typed::HheaTable) {
        let mut hhea = super::tables::hhea::default();
        for record in table.metrics() {
            let keyword = record.keyword();
            match keyword.kind {
                Kind::CaretOffsetKw => hhea.caret_offset = record.metric().parse(),
                Kind::AscenderKw => hhea.ascender = record.metric().parse(),
                Kind::DescenderKw => hhea.descender = record.metric().parse(),
                Kind::LineGapKw => hhea.line_gap = record.metric().parse(),
                other => panic!("bug in parser, unexpected token '{}'", other),
            }
        }
        self.tables.hhea = Some(hhea);
    }

    fn resolve_vhea(&mut self, table: &typed::VheaTable) {
        let mut vhea = super::tables::vhea::default();
        for record in table.metrics() {
            let keyword = record.keyword();
            match keyword.kind {
                Kind::VertTypoAscenderKw => vhea.vert_typo_ascender = record.metric().parse(),
                Kind::VertTypoDescenderKw => vhea.vert_typo_descender = record.metric().parse(),
                Kind::VertTypoLineGapKw => vhea.vert_typo_line_gap = record.metric().parse(),
                other => panic!("bug in parser, unexpected token '{}'", other),
            }
        }
        self.tables.vhea = Some(vhea);

        //FIXME: add vhea to fonttools
        let tag = table.tag();
        self.error(tag.range(), "vhea compilation not implemented");
    }

    fn resolve_vmtx(&mut self, table: &typed::VmtxTable) {
        let mut vmtx = super::tables::vmtx::default();
        for item in table.statements() {
            let glyph = self.resolve_glyph(&item.glyph());
            let value = item.value().parse_signed();
            match item.keyword().kind {
                Kind::VertAdvanceYKw => vmtx.advances_y.push((glyph, value)),
                Kind::VertOriginYKw => vmtx.origins_y.push((glyph, value)),
                _ => unreachable!(),
            }
        }
        self.tables.vmtx = Some(vmtx);
    }

    fn resolve_gdef(&mut self, table: &typed::GdefTable) {
        let mut gdef = super::tables::GDEF::default();
        for statement in table.statements() {
            match statement {
                typed::GdefTableItem::Attach(rule) => {
                    let glyphs = self.resolve_glyph_or_class(&rule.target());
                    let indices = rule
                        .indices()
                        .map(|n| n.parse_unsigned().unwrap())
                        .collect::<Vec<_>>();
                    assert!(!indices.is_empty(), "check this in validation");
                    for glyph in glyphs.iter() {
                        gdef.attach
                            .entry(glyph)
                            .or_default()
                            .extend(indices.iter().copied());
                    }
                }
                typed::GdefTableItem::LigatureCaret(rule) => {
                    let target = rule.target();
                    let glyphs = self.resolve_glyph_or_class(&target);
                    let mut carets: Vec<_> = match rule.values() {
                        typed::LigatureCaretValue::Pos(items) => items
                            .values()
                            .map(|n| n.parse_signed())
                            .map(|p| CaretValue::Format1 { coordinate: p })
                            .collect(),
                        typed::LigatureCaretValue::Index(items) => items
                            .values()
                            .map(|n| n.parse_unsigned())
                            .map(|p| CaretValue::Format2 {
                                pointIndex: p.unwrap(),
                            })
                            .collect(),
                    };
                    carets.sort_by_key(|c| match c {
                        CaretValue::Format1 { coordinate } => *coordinate as i32,
                        CaretValue::Format2 { pointIndex } => *pointIndex as i32,
                        CaretValue::Format3 { coordinate, .. } => *coordinate as i32,
                    });
                    for glyph in glyphs.iter() {
                        gdef.ligature_pos
                            .entry(glyph)
                            .or_insert_with(|| carets.clone());
                        dbg!(&glyph, &carets);
                    }
                }

                typed::GdefTableItem::ClassDef(rule) => {
                    if let Some(glyphs) = rule.base_glyphs() {
                        gdef.add_glyph_class(self.resolve_glyph_class(&glyphs), ClassId::Base);
                    }
                    if let Some(glyphs) = rule.ligature_glyphs() {
                        gdef.add_glyph_class(self.resolve_glyph_class(&glyphs), ClassId::Ligature);
                    }
                    if let Some(glyphs) = rule.mark_glyphs() {
                        gdef.add_glyph_class(self.resolve_glyph_class(&glyphs), ClassId::Mark);
                    }
                    if let Some(glyphs) = rule.component_glyphs() {
                        gdef.add_glyph_class(self.resolve_glyph_class(&glyphs), ClassId::Component);
                    }
                }
            }
        }
        self.tables.GDEF = Some(gdef);
    }

    fn resolve_head(&mut self, table: &typed::HeadTable) {
        let mut head = super::tables::head::default();
        let font_rev = table.statements().last().unwrap().value();
        let float = font_rev.text().parse::<f32>().unwrap();
        head.font_revision = float;
        self.tables.head = Some(head);
    }

    fn resolve_name_spec(&mut self, node: &typed::NameSpec) -> super::tables::NameSpec {
        const WIN_PLATFORM: u16 = 3;
        const MAC_PLATFORM: u16 = 1;
        const WIN_DEFAULT_IDS: (u16, u16) = (1, 0x0409);
        const MAC_DEFAULT_IDS: (u16, u16) = (0, 0);

        let platform_id = node
            .platform_id()
            .map(|n| n.parse().unwrap())
            .unwrap_or(WIN_PLATFORM);

        let (encoding_id, language_id) = match node.platform_and_language_ids() {
            Some((platform, language)) => (platform.parse().unwrap(), language.parse().unwrap()),
            None => match platform_id {
                MAC_PLATFORM => MAC_DEFAULT_IDS,
                WIN_PLATFORM => WIN_DEFAULT_IDS,
                _ => panic!("missed validation"),
            },
        };
        super::tables::NameSpec {
            platform_id,
            encoding_id,
            language_id,
            string: node.string().text.clone(),
        }
    }

    fn resolve_lookup_ref(&mut self, lookup: typed::LookupRef) {
        let id = self
            .lookups
            .get_named(&lookup.label().text)
            .expect("checked in validation pass");
        match self.cur_feature_name {
            Some(feature) => self.add_lookup_to_feature(id, feature),
            None => self.warning(
                lookup.range(),
                "lookup reference outside of feature does nothing",
            ),
        }
    }

    fn resolve_lookup_block(&mut self, lookup: typed::LookupBlock) {
        self.start_lookup_block(lookup.tag());

        //let use_extension = lookup.use_extension().is_some();
        for item in lookup.statements() {
            self.resolve_statement(item);
        }
        self.end_lookup_block();
    }

    fn resolve_statement(&mut self, item: &NodeOrToken) {
        if let Some(script) = typed::Script::cast(item) {
            self.set_script(script);
        } else if let Some(language) = typed::Language::cast(item) {
            self.set_language(language);
        } else if let Some(lookupflag) = typed::LookupFlag::cast(item) {
            self.set_lookup_flag(lookupflag);
        } else if let Some(glyph_def) = typed::GlyphClassDef::cast(item) {
            self.define_glyph_class(glyph_def);
        } else if let Some(glyph_def) = typed::MarkClassDef::cast(item) {
            self.define_mark_class(glyph_def);
        } else if item.kind() == Kind::SubtableNode {
            self.add_subtable_break();
        } else if let Some(lookup) = typed::LookupRef::cast(item) {
            self.resolve_lookup_ref(lookup);
        } else if let Some(lookup) = typed::LookupBlock::cast(item) {
            self.resolve_lookup_block(lookup);
        } else if let Some(rule) = typed::GsubStatement::cast(item) {
            self.add_gsub_statement(rule);
        } else if let Some(rule) = typed::GposStatement::cast(item) {
            self.add_gpos_statement(rule)
        } else {
            let span = match item {
                NodeOrToken::Token(t) => t.range(),
                NodeOrToken::Node(node) => {
                    let range = node.range();
                    let end = range.end.min(range.start + 16);
                    range.start..end
                }
            };
            self.error(span, format!("unhandled statement: '{}'", item.kind()));
        }
    }

    fn define_named_anchor(&mut self, anchor_def: typed::AnchorDef) {
        let anchor_block = anchor_def.anchor();
        let name = anchor_def.name();
        let anchor = match self.resolve_anchor(&anchor_block) {
            Some(a @ Anchor::Coord { .. } | a @ Anchor::Contour { .. }) => a,
            Some(_) => {
                return self.error(
                    anchor_block.range(),
                    "named anchor definition can only be in format A or B",
                )
            }
            None => return,
        };
        if let Some(_prev) = self
            .anchor_defs
            .insert(name.text.clone(), (anchor, anchor_def.range().start))
        {
            self.error(name.range(), "duplicate anchor definition");
        }
    }

    fn resolve_anchor(&mut self, item: &typed::Anchor) -> Option<Anchor> {
        if let Some((x, y)) = item.coords().map(|(x, y)| (x.parse(), y.parse())) {
            if let Some(point) = item.contourpoint() {
                match point.parse_unsigned() {
                    Some(point) => return Some(Anchor::Contour { x, y, point }),
                    None => panic!("negative contourpoint, go fix your parser"),
                }
            } else {
                return Some(Anchor::Coord { x, y });
            }
        } else if let Some(name) = item.name() {
            match self.anchor_defs.get(&name.text) {
                Some((anchor, pos)) if *pos < item.range().start => return Some(*anchor),
                _ => {
                    self.error(name.range(), "anchor is not defined");
                    return None;
                }
            }
        } else if item.null().is_some() {
            return Some(Anchor::Null);
        }
        panic!("bad anchor {:?} go check your parser", item);
    }

    fn resolve_glyph_or_class(&mut self, item: &typed::GlyphOrClass) -> GlyphOrClass {
        match item {
            typed::GlyphOrClass::Glyph(name) => GlyphOrClass::Glyph(self.resolve_glyph_name(name)),
            typed::GlyphOrClass::Cid(cid) => GlyphOrClass::Glyph(self.resolve_cid(cid)),
            typed::GlyphOrClass::Class(class) => {
                GlyphOrClass::Class(self.resolve_glyph_class_literal(class))
            }
            typed::GlyphOrClass::NamedClass(name) => {
                GlyphOrClass::Class(self.resolve_named_glyph_class(name))
            }
            typed::GlyphOrClass::Null(_) => GlyphOrClass::Null,
        }
    }

    fn resolve_glyph(&mut self, item: &typed::Glyph) -> GlyphId {
        match item {
            typed::Glyph::Named(name) => self.resolve_glyph_name(name),
            typed::Glyph::Cid(name) => self.resolve_cid(name),
            typed::Glyph::Null(_) => GlyphId::NOTDEF,
        }
    }

    fn resolve_glyph_class(&mut self, item: &typed::GlyphClass) -> GlyphClass {
        match item {
            typed::GlyphClass::Named(name) => self.resolve_named_glyph_class(name),
            typed::GlyphClass::Literal(lit) => self.resolve_glyph_class_literal(lit),
        }
    }

    fn resolve_glyph_class_literal(&mut self, class: &typed::GlyphClassLiteral) -> GlyphClass {
        let mut glyphs = Vec::new();
        for item in class.items() {
            if let Some(id) =
                typed::GlyphName::cast(item).map(|name| self.resolve_glyph_name(&name))
            {
                glyphs.push(id);
            } else if let Some(id) = typed::Cid::cast(item).map(|cid| self.resolve_cid(&cid)) {
                glyphs.push(id);
            } else if let Some(range) = typed::GlyphRange::cast(item) {
                self.add_glyphs_from_range(&range, &mut glyphs);
            } else if let Some(alias) = typed::GlyphClassName::cast(item) {
                glyphs.extend(self.resolve_named_glyph_class(&alias).items());
            } else {
                panic!("unexptected kind in class literal: '{}'", item.kind());
            }
        }
        glyphs.into()
    }

    fn resolve_named_glyph_class(&mut self, name: &typed::GlyphClassName) -> GlyphClass {
        self.glyph_class_defs
            .get(name.text())
            .cloned()
            .or_else(|| {
                self.mark_classes.get(name.text()).map(|cls| {
                    cls.members
                        .iter()
                        .flat_map(|(glyphs, _)| glyphs.iter())
                        .collect()
                })
            })
            .unwrap()
    }

    fn resolve_glyph_name(&mut self, name: &typed::GlyphName) -> GlyphId {
        self.glyph_map.get(name.text()).unwrap()
    }

    fn resolve_cid(&mut self, cid: &typed::Cid) -> GlyphId {
        self.glyph_map.get(&cid.parse()).unwrap()
    }

    fn add_glyphs_from_range(&mut self, range: &typed::GlyphRange, out: &mut Vec<GlyphId>) {
        let start = range.start();
        let end = range.end();

        match (start.kind, end.kind) {
            (Kind::Cid, Kind::Cid) => {
                if let Err(err) = glyph_range::cid(start, end, |cid| {
                    match self.glyph_map.get(&cid) {
                        Some(id) => out.push(id),
                        None => {
                            // this is techincally allowed, but we error for now
                            self.error(
                                range.range(),
                                format!("Range member '{}' does not exist in font", cid),
                            );
                        }
                    }
                }) {
                    self.error(range.range(), err);
                }
            }
            (Kind::GlyphName, Kind::GlyphName) => {
                if let Err(err) = glyph_range::named(start, end, |name| {
                    match self.glyph_map.get(name) {
                        Some(id) => out.push(id),
                        None => {
                            // this is techincally allowed, but we error for now
                            self.error(
                                range.range(),
                                format!("Range member '{}' does not exist in font", name),
                            );
                        }
                    }
                }) {
                    self.error(range.range(), err);
                }
            }
            (_, _) => self.error(range.range(), "Invalid types in glyph range"),
        }
    }
}

fn sequence_enumerator(sequence: &[GlyphOrClass]) -> Vec<Vec<u16>> {
    assert!(sequence.len() >= 2);
    let split = sequence.split_first();
    let mut result = Vec::new();
    let (left, right) = split.unwrap();
    sequence_enumerator_impl(Vec::new(), left, right, &mut result);
    result
}

fn sequence_enumerator_impl(
    prefix: Vec<u16>,
    left: &GlyphOrClass,
    right: &[GlyphOrClass],
    acc: &mut Vec<Vec<u16>>,
) {
    for glyph in left.iter() {
        let mut prefix = prefix.clone();
        prefix.push(glyph.to_raw());

        match right.split_first() {
            Some((head, tail)) => sequence_enumerator_impl(prefix, head, tail, acc),
            None => acc.push(prefix),
        }
    }
}

fn make_ctx_glyphs(item: &GlyphOrClass) -> BTreeSet<u16> {
    item.iter().map(|g| g.to_raw()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_enumerator_smoke_test() {
        let sequence = vec![
            GlyphOrClass::Glyph(GlyphId::from_raw(1)),
            GlyphOrClass::Class(
                [2_u16, 3, 4]
                    .iter()
                    .copied()
                    .map(GlyphId::from_raw)
                    .collect(),
            ),
            GlyphOrClass::Class([8, 9].iter().copied().map(GlyphId::from_raw).collect()),
        ];

        assert_eq!(
            sequence_enumerator(&sequence),
            vec![
                vec![1, 2, 8],
                vec![1, 2, 9],
                vec![1, 3, 8],
                vec![1, 3, 9],
                vec![1, 4, 8],
                vec![1, 4, 9],
            ]
        );
    }
}
