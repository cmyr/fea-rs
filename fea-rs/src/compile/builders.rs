//! Builders for layout tables

use std::collections::{BTreeMap, BTreeSet};

use font_types::GlyphId;
use write_fonts::tables::{
    gpos::{
        self, AnchorTable, MarkArray, MarkRecord, PairSet, PairValueRecord, ValueFormat,
        ValueRecord,
    },
    layout::{CoverageTable, CoverageTableBuilder},
};

type MarkClass = u16;

pub trait Builder {
    type Output;
    fn build(self) -> Result<Self::Output, ()>;
}

#[derive(Clone, Debug, Default)]
struct SinglePosSubtable {
    format: ValueFormat,
    items: BTreeMap<GlyphId, ValueRecord>,
}

#[derive(Clone, Debug, Default)]
pub struct SinglePosBuilder {
    subtables: Vec<SinglePosSubtable>,
}

impl SinglePosBuilder {
    //TODO: should we track the valueformat here?
    pub fn insert(&mut self, glyph: GlyphId, record: ValueRecord) {
        self.get_subtable(record.format())
            .items
            .insert(glyph, record);
    }

    fn get_subtable(&mut self, format: ValueFormat) -> &mut SinglePosSubtable {
        if self.subtables.last().map(|sub| sub.format) != Some(format) {
            self.subtables.push(SinglePosSubtable {
                format,
                items: BTreeMap::new(),
            });
        }
        self.subtables.last_mut().unwrap()
    }
}

impl Builder for SinglePosBuilder {
    type Output = Vec<gpos::SinglePos>;

    fn build(self) -> Result<Self::Output, ()> {
        self.subtables.into_iter().map(Builder::build).collect()
    }
}

impl Builder for SinglePosSubtable {
    type Output = gpos::SinglePos;

    fn build(self) -> Result<Self::Output, ()> {
        let first_value = self.items.values().next().unwrap();
        let format_1 = self.items.values().all(|val| val == first_value);
        let coverage: CoverageTableBuilder = self.items.keys().copied().collect();
        if format_1 {
            Ok(gpos::SinglePos::format_1(
                coverage.build(),
                first_value.to_owned(),
            ))
        } else {
            Ok(gpos::SinglePos::format_2(
                coverage.build(),
                self.items.into_values().collect(),
            ))
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PairPosBuilder {
    items: BTreeMap<GlyphId, BTreeMap<GlyphId, (ValueRecord, ValueRecord)>>,
}

impl PairPosBuilder {
    pub fn insert(
        &mut self,
        glyph1: GlyphId,
        record1: ValueRecord,
        glyph2: GlyphId,
        record2: ValueRecord,
    ) {
        self.items
            .entry(glyph1)
            .or_default()
            .insert(glyph2, (record1, record2));
    }
}

impl Builder for PairPosBuilder {
    type Output = Vec<gpos::PairPos>;

    //FIXME: this always uses format 1.
    fn build(self) -> Result<Self::Output, ()> {
        let mut split_by_format = BTreeMap::new();
        for (g1, map) in self.items {
            for (g2, (v1, v2)) in map {
                split_by_format
                    .entry((v1.format(), v2.format()))
                    .or_insert(BTreeMap::default())
                    .entry(g1)
                    .or_insert(Vec::new())
                    .push(PairValueRecord::new(g2, v1, v2));
            }
        }

        Ok(split_by_format
            .into_iter()
            .map(|(_, map)| {
                let coverage: CoverageTableBuilder = map.keys().copied().collect();
                let pair_sets = map.into_values().map(PairSet::new).collect();
                gpos::PairPos::format_1(coverage.build(), pair_sets)
            })
            .collect())
    }
}

#[derive(Clone, Debug, Default)]
pub struct CursivePosBuilder {
    items: BTreeMap<GlyphId, gpos::EntryExitRecord>,
}

impl CursivePosBuilder {
    pub fn insert(
        &mut self,
        glyph: GlyphId,
        entry: Option<AnchorTable>,
        exit: Option<AnchorTable>,
    ) {
        let record = gpos::EntryExitRecord::new(entry, exit);
        self.items.insert(glyph, record);
    }
}

impl Builder for CursivePosBuilder {
    type Output = Vec<gpos::CursivePosFormat1>;

    fn build(self) -> Result<Self::Output, ()> {
        let coverage: CoverageTableBuilder = self.items.keys().copied().collect();
        let records = self.items.into_values().collect();
        Ok(vec![gpos::CursivePosFormat1::new(
            coverage.build(),
            records,
        )])
    }
}

// shared between several tables
#[derive(Clone, Debug, Default)]
struct MarkList(BTreeMap<GlyphId, MarkRecord>);

impl MarkList {
    fn insert(
        &mut self,
        glyph: GlyphId,
        class: MarkClass,
        anchor: AnchorTable,
    ) -> Result<(), PreviouslyAssignedClass> {
        match self
            .0
            .insert(glyph, MarkRecord::new(class, anchor))
            .map(|rec| rec.mark_class)
        {
            Some(old_class) if old_class != class => Err(PreviouslyAssignedClass {
                glyph_id: glyph,
                class: old_class,
            }),
            _ => Ok(()),
        }
    }

    fn glyphs(&self) -> impl Iterator<Item = GlyphId> + Clone + '_ {
        self.0.keys().copied()
    }
}

impl Builder for MarkList {
    type Output = (CoverageTable, MarkArray);

    fn build(self) -> Result<Self::Output, ()> {
        let coverage = self.0.keys().copied().collect::<CoverageTableBuilder>();
        let array = MarkArray::new(self.0.into_values().collect());
        Ok((coverage.build(), array))
    }
}

#[derive(Clone, Debug, Default)]
pub struct MarkToBaseBuilder {
    marks: MarkList,
    mark_classes: BTreeSet<MarkClass>,
    bases: BTreeMap<GlyphId, Vec<(MarkClass, AnchorTable)>>,
}

/// An error indicating a given glyph is has be
pub struct PreviouslyAssignedClass {
    pub glyph_id: GlyphId,
    pub class: MarkClass,
}

impl MarkToBaseBuilder {
    /// Add a new mark glyph.
    ///
    /// If this glyph already exists in another mark class, we return the
    /// previous class; this is likely an error.
    pub fn insert_mark(
        &mut self,
        glyph: GlyphId,
        class: MarkClass,
        anchor: AnchorTable,
    ) -> Result<(), PreviouslyAssignedClass> {
        self.mark_classes.insert(class);
        self.marks.insert(glyph, class, anchor)
    }

    pub fn insert_base(&mut self, glyph: GlyphId, class: MarkClass, anchor: AnchorTable) {
        self.bases.entry(glyph).or_default().push((class, anchor))
    }

    pub fn base_glyphs(&self) -> impl Iterator<Item = GlyphId> + Clone + '_ {
        self.bases.keys().copied()
    }

    pub fn mark_glyphs(&self) -> impl Iterator<Item = GlyphId> + Clone + '_ {
        self.marks.glyphs()
    }
}

impl Builder for MarkToBaseBuilder {
    type Output = Vec<gpos::MarkBasePosFormat1>;

    fn build(self) -> Result<Self::Output, ()> {
        let MarkToBaseBuilder {
            marks,
            bases,
            mark_classes,
        } = self;

        let (mark_coverage, mark_array) = marks.build()?;
        let base_coverage = bases.keys().copied().collect::<CoverageTableBuilder>();
        let base_records = bases
            .into_values()
            .map(|anchors| {
                let mut anchor_offsets: Vec<Option<AnchorTable>> = Vec::new();
                anchor_offsets.resize(mark_classes.len(), None);
                for (class, anchor) in anchors {
                    let class_idx = mark_classes.iter().position(|c| c == &class).unwrap();
                    anchor_offsets[class_idx] = Some(anchor);
                }
                gpos::BaseRecord::new(anchor_offsets)
            })
            .collect();
        let base_array = gpos::BaseArray::new(base_records);
        Ok(vec![gpos::MarkBasePosFormat1::new(
            mark_coverage,
            base_coverage.build(),
            mark_array,
            base_array,
        )])
    }
}

#[derive(Clone, Debug, Default)]
pub struct MarkToLigBuilder {
    marks: MarkList,
    mark_classes: BTreeSet<MarkClass>,
    ligatures: BTreeMap<GlyphId, Vec<BTreeMap<MarkClass, AnchorTable>>>,
}

impl MarkToLigBuilder {
    pub fn insert_mark(
        &mut self,
        glyph: GlyphId,
        class: MarkClass,
        anchor: AnchorTable,
    ) -> Result<(), PreviouslyAssignedClass> {
        self.mark_classes.insert(class);
        self.marks.insert(glyph, class, anchor)
    }

    pub fn add_lig(&mut self, glyph: GlyphId, components: Vec<BTreeMap<MarkClass, AnchorTable>>) {
        self.ligatures.insert(glyph, components);
    }

    pub fn mark_glyphs(&self) -> impl Iterator<Item = GlyphId> + Clone + '_ {
        self.marks.glyphs()
    }

    pub fn lig_glyphs(&self) -> impl Iterator<Item = GlyphId> + Clone + '_ {
        self.ligatures.keys().copied()
    }
}

impl Builder for MarkToLigBuilder {
    type Output = Vec<gpos::MarkLigPosFormat1>;

    fn build(self) -> Result<Self::Output, ()> {
        let MarkToLigBuilder {
            marks,
            mark_classes,
            ligatures,
        } = self;

        let (mark_coverage, mark_array) = marks.build()?;
        // LigArray:
        // - [LigatureAttach] (one per ligature glyph)
        //    - [ComponentRecord] (one per component)
        //    - [Anchor] (one per mark-class)
        let ligature_coverage = ligatures.keys().copied().collect::<CoverageTableBuilder>();
        let ligature_array = ligatures
            .into_values()
            .map(|components| {
                let comp_records = components
                    .into_iter()
                    .map(|anchors| {
                        let mut anchor_offsets: Vec<Option<AnchorTable>> = Vec::new();
                        anchor_offsets.resize(mark_classes.len(), None);
                        for (class, anchor) in anchors {
                            let class_idx = mark_classes.iter().position(|c| c == &class).unwrap();
                            anchor_offsets[class_idx] = Some(anchor);
                        }
                        gpos::ComponentRecord::new(anchor_offsets)
                    })
                    .collect();
                gpos::LigatureAttach::new(comp_records)
            })
            .collect();
        let ligature_array = gpos::LigatureArray::new(ligature_array);
        Ok(vec![gpos::MarkLigPosFormat1::new(
            mark_coverage,
            ligature_coverage.build(),
            mark_array,
            ligature_array,
        )])
    }
}

#[derive(Clone, Debug, Default)]
pub struct MarkToMarkBuilder {
    attaching_marks: MarkList,
    mark_classes: BTreeSet<MarkClass>,
    base_marks: BTreeMap<GlyphId, Vec<(MarkClass, AnchorTable)>>,
}

impl MarkToMarkBuilder {
    pub fn insert_mark(
        &mut self,
        glyph: GlyphId,
        class: MarkClass,
        anchor: AnchorTable,
    ) -> Result<(), PreviouslyAssignedClass> {
        self.mark_classes.insert(class);
        self.attaching_marks.insert(glyph, class, anchor)
    }

    pub fn insert_base(&mut self, glyph: GlyphId, class: MarkClass, anchor: AnchorTable) {
        self.base_marks
            .entry(glyph)
            .or_default()
            .push((class, anchor))
    }

    pub fn mark1_glyphs(&self) -> impl Iterator<Item = GlyphId> + Clone + '_ {
        self.attaching_marks.glyphs()
    }

    pub fn mark2_glyphs(&self) -> impl Iterator<Item = GlyphId> + Clone + '_ {
        self.base_marks.keys().copied()
    }
}

impl Builder for MarkToMarkBuilder {
    type Output = Vec<gpos::MarkMarkPosFormat1>;

    fn build(self) -> Result<Self::Output, ()> {
        let MarkToMarkBuilder {
            attaching_marks,
            base_marks,
            mark_classes,
        } = self;

        let (mark_coverage, mark_array) = attaching_marks.build()?;
        let mark2_coverage = base_marks.keys().copied().collect::<CoverageTableBuilder>();
        let mark2_records = base_marks
            .into_values()
            .map(|anchors| {
                let mut anchor_offsets: Vec<Option<AnchorTable>> = Vec::new();
                anchor_offsets.resize(mark_classes.len(), None);
                for (class, anchor) in anchors {
                    let class_idx = mark_classes.iter().position(|c| c == &class).unwrap();
                    anchor_offsets[class_idx] = Some(anchor);
                }
                gpos::Mark2Record::new(anchor_offsets)
            })
            .collect();
        let mark2array = gpos::Mark2Array::new(mark2_records);
        Ok(vec![gpos::MarkMarkPosFormat1::new(
            mark_coverage,
            mark2_coverage.build(),
            mark_array,
            mark2array,
        )])
    }
}

#[derive(Clone, Debug, Default)]
pub struct SingleSubBuilder {
    items: BTreeMap<GlyphId, GlyphId>,
}

impl SingleSubBuilder {
    pub fn insert(&mut self, target: GlyphId, replacement: GlyphId) {
        self.items.insert(target, replacement);
    }
}

#[derive(Clone, Debug, Default)]
pub struct MultipleSubBuilder {
    items: BTreeMap<GlyphId, Vec<GlyphId>>,
}

impl MultipleSubBuilder {
    pub fn insert(&mut self, target: GlyphId, replacement: Vec<GlyphId>) {
        self.items.insert(target, replacement);
    }
}

#[derive(Clone, Debug, Default)]
pub struct AlternateSubBuilder {
    items: BTreeMap<GlyphId, Vec<GlyphId>>,
}

impl AlternateSubBuilder {
    pub fn insert(&mut self, target: GlyphId, replacement: Vec<GlyphId>) {
        self.items.insert(target, replacement);
    }
}

#[derive(Clone, Debug, Default)]
pub struct LigatureSubBuilder {
    items: BTreeMap<Vec<GlyphId>, GlyphId>,
}

impl LigatureSubBuilder {
    pub fn insert(&mut self, target: Vec<GlyphId>, replacement: GlyphId) {
        self.items.insert(target, replacement);
    }
}

//#[derive(Clone, Debug, Default)]
//pub struct SubBuilder {}
