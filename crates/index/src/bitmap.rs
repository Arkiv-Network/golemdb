use std::ops::{BitAndAssign, BitOrAssign};

use roaring::RoaringTreemap;

use crate::{BitmapContainer, BitmapError};

/// A materialized query bitmap assembled from storage chunks.
///
/// Owns the chunk-to-record-ID conversion, not storage or Merkle hashing.
/// Set operations are operators on `Bitmap` (`|=` is union, `&=` is
/// intersection), so callers need not name Roaring; [`Self::treemap`] reads
/// the record IDs. The default is the empty bitmap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bitmap {
    values: RoaringTreemap,
}

impl Bitmap {
    /// Assemble chunks in strictly increasing path order, as produced by trie
    /// traversal. Empty working chunks are omitted, but their paths must still
    /// be unique and ordered. No sorting or duplicate overwrites are hidden here.
    pub fn from_containers(
        containers: impl IntoIterator<Item = BitmapContainer>,
    ) -> Result<Self, BitmapError> {
        let mut values = RoaringTreemap::new();
        let mut previous = None;
        for container in containers {
            let path = container.path();
            if let Some(prev) = previous {
                if path == prev {
                    return Err(BitmapError::DuplicatePath { path });
                }
                if path < prev {
                    return Err(BitmapError::UnsortedPaths {
                        previous: prev,
                        path,
                    });
                }
            }
            previous = Some(path);
            values
                .append(container.iter())
                .expect("validated paths and offsets are strictly ordered");
        }
        Ok(Self { values })
    }

    pub fn treemap(&self) -> &RoaringTreemap {
        &self.values
    }

    pub fn into_treemap(self) -> RoaringTreemap {
        self.values
    }
}

/// Union: `self` gains every record ID of `other`.
impl BitOrAssign for Bitmap {
    fn bitor_assign(&mut self, other: Self) {
        self.values |= other.values;
    }
}

/// Intersection: `self` keeps only the record IDs `other` also holds.
impl BitAndAssign for Bitmap {
    fn bitand_assign(&mut self, other: Self) {
        self.values &= other.values;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    /// A bitmap of `ids`, built through the treemap.
    fn bitmap_of(ids: impl IntoIterator<Item = u64>) -> Bitmap {
        Bitmap {
            values: ids.into_iter().collect(),
        }
    }

    #[test]
    fn intersection_keeps_the_common_ids_across_containers() {
        let mut intersection = bitmap_of([1, 2, 1 << 40, u64::MAX]);
        intersection &= bitmap_of([2, 3, 1 << 40, u64::MAX]);
        assert_eq!(intersection, bitmap_of([2, 1 << 40, u64::MAX]));
    }

    #[test]
    fn intersection_of_disjoint_bitmaps_is_empty() {
        let mut intersection = bitmap_of([1, 1 << 40]);
        intersection &= bitmap_of([2, 1 << 41]);
        assert!(intersection.treemap().is_empty());
    }

    #[test]
    fn intersection_with_the_empty_bitmap_is_empty() {
        let mut left = Bitmap::default();
        left &= bitmap_of([7, 1 << 32]);
        assert_eq!(left, Bitmap::default());
        let mut right = bitmap_of([7, 1 << 32]);
        right &= Bitmap::default();
        assert_eq!(right, Bitmap::default());
    }

    /// IDs from a small domain spread over four containers, so two sets
    /// overlap often.
    fn ids() -> impl Strategy<Value = BTreeSet<u64>> {
        let id = (0u64..4, 0u64..64).prop_map(|(container, offset)| (container << 40) | offset);
        prop::collection::btree_set(id, 0..100)
    }

    proptest! {
        #[test]
        fn intersection_is_set_intersection(a in ids(), b in ids()) {
            let mut intersection = bitmap_of(a.iter().copied());
            intersection &= bitmap_of(b.iter().copied());
            prop_assert_eq!(intersection, bitmap_of(a.intersection(&b).copied()));
        }
    }
}
