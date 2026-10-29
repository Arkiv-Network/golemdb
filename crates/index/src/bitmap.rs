use std::ops::{BitAndAssign, BitOrAssign};

use roaring::{MultiOps, RoaringTreemap};

use crate::{BitmapContainer, BitmapError, CHUNK_BITS};

/// IDs per container from which Roaring stores a bitmap, not an array.
const DENSE_SET: u64 = 4096;

/// IDs per spanned container from which a batch merges in one multi-way union.
const DENSE_UNION: u64 = 512;

/// A materialized query bitmap assembled from storage chunks.
///
/// Owns the chunk-to-record-ID conversion, not storage or Merkle hashing.
/// Set operations are operators on `Bitmap` (`|=` is union, `&=` is
/// intersection), with [`Self::union_all`] for many unions at once, so
/// callers need not name Roaring; [`Self::treemap`] reads
/// the record IDs, and [`Self::iter_skipping`] reads them past a rank. The
/// default is the empty bitmap.
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

    /// Whether its containers are mostly bitmaps, which `|=` merges in place.
    pub fn is_dense(&self) -> bool {
        ids_per_container(self.values.len(), self.values.min(), self.values.max()) >= DENSE_SET
    }

    /// Add the union of `bitmaps`: in one multi-way union with this bitmap
    /// when dense, otherwise one by one.
    pub fn union_all(&mut self, bitmaps: Vec<Self>) {
        let ids = bitmaps.iter().map(|bitmap| bitmap.values.len()).sum();
        let min = bitmaps
            .iter()
            .filter_map(|bitmap| bitmap.values.min())
            .min();
        let max = bitmaps
            .iter()
            .filter_map(|bitmap| bitmap.values.max())
            .max();
        if ids_per_container(ids, min, max) < DENSE_UNION {
            *self |= bitmaps
                .into_iter()
                .fold(Self::default(), |mut union, bitmap| {
                    union |= bitmap;
                    union
                });
            return;
        }
        let current = std::mem::take(&mut self.values);
        self.values = std::iter::once(current)
            .chain(bitmaps.into_iter().map(|bitmap| bitmap.values))
            .union();
    }

    pub fn treemap(&self) -> &RoaringTreemap {
        &self.values
    }

    pub fn into_treemap(self) -> RoaringTreemap {
        self.values
    }

    /// The record IDs in ascending order past the first `n`, as
    /// `treemap().iter().skip(n)` yields them. The first one is found by its
    /// rank, so the skipped IDs are not walked one by one. Empty when the
    /// bitmap holds `n` IDs or fewer.
    pub fn iter_skipping(&self, n: u64) -> impl Iterator<Item = u64> + '_ {
        self.values.select(n).into_iter().flat_map(move |first| {
            let mut ids = self.values.iter();
            ids.advance_to(first);
            ids
        })
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

/// `ids` per container spanned by `min..=max`; zero when empty.
fn ids_per_container(ids: u64, min: Option<u64>, max: Option<u64>) -> u64 {
    match (min, max) {
        (Some(min), Some(max)) => ids / ((max >> CHUNK_BITS) - (min >> CHUNK_BITS) + 1),
        _ => 0,
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

    /// IDs sharing a Roaring container, in a second container of the same
    /// treemap bucket, and in three more buckets, up to the greatest ID.
    const SPREAD: [u64; 6] = [1, 2, 1 << 16, 1 << 32, 1 << 40, u64::MAX];

    #[test]
    fn skipping_no_id_yields_them_all() {
        assert!(bitmap_of(SPREAD).iter_skipping(0).eq(SPREAD));
    }

    #[test]
    fn skipping_passes_the_first_ids_across_containers() {
        let bitmap = bitmap_of(SPREAD);
        for n in 1..SPREAD.len() {
            let rest = SPREAD[n..].iter().copied();
            assert!(bitmap.iter_skipping(n as u64).eq(rest), "{n}");
        }
    }

    #[test]
    fn skipping_every_id_or_more_yields_none() {
        let bitmap = bitmap_of(SPREAD);
        for n in [SPREAD.len() as u64, SPREAD.len() as u64 + 1, u64::MAX] {
            assert_eq!(bitmap.iter_skipping(n).next(), None, "{n}");
        }
        assert_eq!(Bitmap::default().iter_skipping(0).next(), None);
    }

    proptest! {
        #[test]
        fn skipping_is_iterator_skip(ids in ids(), n in 0u64..110) {
            let bitmap = bitmap_of(ids.iter().copied());
            let expected = ids.iter().copied().skip(n as usize);
            prop_assert!(bitmap.iter_skipping(n).eq(expected));
        }
    }
}
