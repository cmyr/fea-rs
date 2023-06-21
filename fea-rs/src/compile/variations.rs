//! compiling variable fonts

use write_fonts::types::{F2Dot14, Fixed, Tag};

/// A trait for providing variable font information to the compiler.
///
/// In order to compile a variable font, we need to know what axes
/// exist, what ranges are valid, how to map from user to normalized coordinates,
/// and potentially other things that are not part of the FEA file.
///
/// This trait abstracts over that info.
pub trait VariationInfo {
    /// If the tag is an axis in this font, return the min/max values from fvar
    fn axis_info(&self, axis_tag: Tag) -> Option<AxisInfo>;

    /// Return the normalized value for a user coordinate for the given axis.
    fn normalize_coordinate(&self, axis_tag: Tag, value: Fixed) -> F2Dot14;
}

/// Information about a paritcular axis in a variable font.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AxisInfo {
    /// The index in the fvar table of this axis
    pub index: u16,
    /// The minimum value for this axis, in user coordinates
    pub min_value: Fixed,
    /// The default value for this axis, in user coordinates
    pub default_value: Fixed,
    /// The maximum value for this axis, in user coordinates
    pub max_value: Fixed,
}

// For testing: a simple list of axes
#[derive(Clone, Debug, Default)]
pub(crate) struct MockVariationInfo {
    axes: Vec<(Tag, AxisInfo)>,
}

impl MockVariationInfo {
    /// input is a tuple of (tag, min, default, max)
    #[cfg(any(test, feature = "test"))]
    pub(crate) fn new(raw: &[(&str, i16, i16, i16)]) -> Self {
        Self {
            axes: raw
                .iter()
                .enumerate()
                .map(|(i, (tag, min, default, max))| {
                    (
                        Tag::new_checked(tag.as_bytes()).unwrap(),
                        AxisInfo {
                            index: i as u16,
                            min_value: Fixed::from_i32(*min as _),
                            default_value: Fixed::from_i32(*default as _),
                            max_value: Fixed::from_i32(*max as _),
                        },
                    )
                })
                .collect(),
        }
    }
}

impl VariationInfo for MockVariationInfo {
    fn axis_info(&self, axis_tag: Tag) -> Option<AxisInfo> {
        self.axes.iter().find_map(
            |(tag, axis)| {
                if *tag == axis_tag {
                    Some(*axis)
                } else {
                    None
                }
            },
        )
    }

    fn normalize_coordinate(&self, axis_tag: Tag, value: Fixed) -> F2Dot14 {
        let Some(AxisInfo { min_value, default_value, max_value, .. }) = self.axis_info(axis_tag) else { return F2Dot14::ZERO };

        use core::cmp::Ordering::*;
        // Make sure max is >= min to avoid potential panic in clamp.
        let max_value = max_value.max(min_value);
        let value = value.clamp(min_value, max_value);
        let value = match value.cmp(&default_value) {
            Less => -((default_value - value) / (default_value - min_value)),
            Greater => (value - default_value) / (max_value - default_value),
            Equal => Fixed::ZERO,
        };
        value.clamp(-Fixed::ONE, Fixed::ONE).to_f2dot14()
    }
}