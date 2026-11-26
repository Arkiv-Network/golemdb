//! [`Regions`]: the containers a set of record IDs occupies, so that a read
//! can be confined to them.

use crate::{Bitmap, ContainerId};

/// The [`ContainerId`]s a set of record IDs occupies, ascending and each
/// once: the 48-bit regions of 65 536 record IDs that hold at least one of
/// them. A read confined to them skips every other container of a term,
/// none of which can hold one of the set's IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Regions(Vec<ContainerId>);

impl Regions {
    /// The regions `bitmap`'s record IDs occupy. Steps once per region, not
    /// once per record ID.
    pub fn of(bitmap: &Bitmap) -> Self {
        let mut ids = bitmap.treemap().iter();
        let mut regions = Vec::new();
        while let Some(id) = ids.next() {
            let (region, _) = ContainerId::split(id);
            regions.push(region);
            match region.get().checked_add(1).and_then(ContainerId::new) {
                Some(next) => ids.advance_to(next.record_id(0)),
                // The last region: no record ID lies past it.
                None => break,
            }
        }
        Self(regions)
    }

    /// The regions, ascending.
    pub fn as_slice(&self) -> &[ContainerId] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;

    use super::*;
    use crate::BitmapContainer;

    /// A bitmap of `ids`, grouped into containers as the index stores them.
    fn bitmap_of(ids: impl IntoIterator<Item = u64>) -> Bitmap {
        let mut containers: BTreeMap<ContainerId, Vec<u16>> = BTreeMap::new();
        for id in ids {
            let (region, offset) = ContainerId::split(id);
            containers.entry(region).or_default().push(offset);
        }
        Bitmap::from_containers(
            containers
                .into_iter()
                .map(|(region, offsets)| BitmapContainer::from_values(region, offsets)),
        )
        .expect("unique, ordered containers")
    }

    fn numbers(regions: &Regions) -> Vec<u64> {
        regions
            .as_slice()
            .iter()
            .map(|region| region.get())
            .collect()
    }

    #[test]
    fn each_occupied_region_once_in_order() {
        let ids = [1, 2, u64::from(u16::MAX), 1 << 16, 5 << 16, 1 << 40];
        assert_eq!(numbers(&Regions::of(&bitmap_of(ids))), [0, 1, 5, 1 << 24]);
        assert!(Regions::of(&Bitmap::default()).is_empty());
        // The last region, with no region after it.
        let last = Regions::of(&bitmap_of([u64::MAX - 1, u64::MAX]));
        assert_eq!(last.as_slice(), [ContainerId::MAX]);
    }

    proptest! {
        #[test]
        fn regions_are_the_distinct_high_bits(
            ids in prop::collection::btree_set(
                prop_oneof![0..1u64 << 20, any::<u64>()],
                0..200,
            ),
        ) {
            let expected: Vec<u64> = ids.iter().map(|id| id >> 16).collect::<BTreeSet<_>>().into_iter().collect();
            prop_assert_eq!(numbers(&Regions::of(&bitmap_of(ids))), expected);
        }
    }
}
