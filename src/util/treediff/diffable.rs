/// Diffable structs.
pub trait Diffable {
    /// The output of diffing two structs.
    type Output;

    /// Diffs `self` against `newer`.
    fn diff(&self, newer: &Self) -> Self::Output;
}

pub struct StyleDiff {
    pub flex_direction: Option<taffy::FlexDirection>,
    pub flex_basis: Option<taffy::Dimension>,
    pub flex_grow: Option<f32>,
    pub flex_shrink: Option<f32>,
    pub margin: Option<taffy::Rect<taffy::LengthPercentageAuto>>,
    pub size: Option<taffy::Size<taffy::Dimension>>,
    pub min_size: Option<taffy::Size<taffy::Dimension>>,
    pub max_size: Option<taffy::Size<taffy::Dimension>>,
}

impl Diffable for taffy::Style {
    type Output = StyleDiff;

    fn diff(&self, newer: &Self) -> Self::Output {
        StyleDiff {
            flex_direction: (self.flex_direction != newer.flex_direction)
                .then_some(newer.flex_direction),
            flex_basis: (self.flex_basis != newer.flex_basis).then_some(newer.flex_basis),
            flex_grow: (self.flex_grow != newer.flex_grow).then_some(newer.flex_grow),
            flex_shrink: (self.flex_shrink != newer.flex_shrink).then_some(newer.flex_shrink),
            margin: (self.margin != newer.margin).then_some(newer.margin),
            size: (self.size != newer.size).then_some(newer.size),
            min_size: (self.min_size != newer.min_size).then_some(newer.min_size),
            max_size: (self.max_size != newer.max_size).then_some(newer.max_size),
        }
    }
}
