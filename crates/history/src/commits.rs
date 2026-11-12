use roaring::RoaringTreemap;

/// A full 64-bit set of modifying commits, independent of item/payload codecs.
///
/// An empty in-memory set represents a missing history row. Persisted rows must
/// be nonempty. The wire format is the pinned RoaringTreemap format: a little-
/// endian u64 partition count, then ascending u32 high parts and portable
/// Roaring bitmaps. Each bitmap removes run compression, then optimizes before
/// serialization. No empty partitions, alternate encodings or trailing bytes
/// are accepted. This is a history codec, not an index container codec.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitSet(RoaringTreemap);

#[derive(Debug, thiserror::Error)]
pub enum CommitSetError {
    #[error("invalid Roaring commit set: {0}")]
    Decode(#[from] std::io::Error),
    #[error("an empty commit set must have no history row")]
    Empty,
    #[error("noncanonical commit set encoding")]
    NonCanonical,
}

impl CommitSet {
    /// Decode a row already obtained from storage, for example during a scan.
    pub fn decode(bytes: &[u8]) -> Result<Self, CommitSetError> {
        let mut input = bytes;
        let set = Self(RoaringTreemap::deserialize_from(&mut input)?);
        if !input.is_empty() || set.encode()? != bytes {
            return Err(CommitSetError::NonCanonical);
        }
        Ok(set)
    }

    /// Canonical persisted bytes. Delete the row instead of encoding an empty set.
    pub fn encode(&self) -> Result<Vec<u8>, CommitSetError> {
        if self.0.is_empty() {
            return Err(CommitSetError::Empty);
        }
        // Normalizing each partition also removes any empty partition. Comparing
        // these bytes on decode catches duplicate/unordered high parts, which
        // RoaringTreemap's decoder otherwise inserts into a sorted map.
        let mut out = Vec::new();
        let count = self.0.bitmaps().filter(|(_, b)| !b.is_empty()).count() as u64;
        out.extend_from_slice(&count.to_le_bytes());
        for (high, bitmap) in self.0.bitmaps().filter(|(_, b)| !b.is_empty()) {
            let mut bitmap = bitmap.clone();
            bitmap.remove_run_compression();
            bitmap.optimize();
            out.extend_from_slice(&high.to_le_bytes());
            bitmap
                .serialize_into(&mut out)
                .expect("writing to Vec cannot fail");
        }
        Ok(out)
    }

    /// First modifying commit strictly after target. Seeking skips earlier
    /// values without visiting each; the maximum target has no successor.
    pub fn first_after(&self, target: u64) -> Option<u64> {
        let mut commits = self.0.iter();
        commits.advance_to(target.checked_add(1)?);
        commits.next()
    }

    pub fn latest(&self) -> Option<u64> {
        self.0.max()
    }

    pub(crate) fn insert(&mut self, commit: u64) {
        self.0.insert(commit);
    }
}

impl FromIterator<u64> for CommitSet {
    fn from_iter<T: IntoIterator<Item = u64>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    #[test]
    fn successor_is_strict_across_all_commit_boundaries() {
        let points = [0, 1, u32::MAX as u64, 1 << 32, u64::MAX - 1, u64::MAX];
        let set: CommitSet = points.into_iter().collect();
        for target in [
            0,
            1,
            2,
            u32::MAX as u64 - 1,
            u32::MAX as u64,
            1 << 32,
            u64::MAX - 1,
            u64::MAX,
        ] {
            assert_eq!(
                set.first_after(target),
                points.into_iter().find(|&c| c > target)
            );
        }
        assert_eq!(set.latest(), Some(u64::MAX));
        assert_eq!(CommitSet::default().first_after(0), None);
        let singleton: CommitSet = [10].into_iter().collect();
        assert_eq!(singleton.first_after(9), Some(10));
        assert_eq!(singleton.first_after(10), None);
    }

    #[test]
    fn persisted_singleton_vector() {
        // One high-u32 partition (0), one low-u16 array container (key 0),
        // cardinality minus one = 0, data offset = 16, low value = 42.
        let expected = [
            1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x3a, 0x30, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 16, 0, 0,
            0, 42, 0,
        ];
        let set: CommitSet = [42].into_iter().collect();
        assert_eq!(set.encode().unwrap(), expected);
        assert_eq!(CommitSet::decode(&expected).unwrap(), set);
    }

    #[test]
    fn rejects_empty_truncated_trailing_and_invalid_roaring() {
        assert!(matches!(
            CommitSet::default().encode(),
            Err(CommitSetError::Empty)
        ));
        assert!(matches!(
            CommitSet::decode(&[0; 8]),
            Err(CommitSetError::Empty)
        ));
        let set: CommitSet = [42].into_iter().collect();
        let bytes = set.encode().unwrap();
        for end in 0..bytes.len() {
            assert!(
                CommitSet::decode(&bytes[..end]).is_err(),
                "truncated at {end}"
            );
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(
            CommitSet::decode(&trailing),
            Err(CommitSetError::NonCanonical)
        ));
        let mut bad = bytes;
        bad[12] = 0xff; // Portable bitmap cookie.
        assert!(matches!(
            CommitSet::decode(&bad),
            Err(CommitSetError::Decode(_))
        ));
    }

    #[test]
    fn rejects_duplicate_unordered_and_empty_partitions() {
        let a = [1].into_iter().collect::<CommitSet>().encode().unwrap();
        let b = [(1 << 32) + 2]
            .into_iter()
            .collect::<CommitSet>()
            .encode()
            .unwrap();
        for (left, right) in [(&a[8..], &a[8..]), (&b[8..], &a[8..])] {
            let bytes = [&2u64.to_le_bytes()[..], left, right].concat();
            assert!(matches!(
                CommitSet::decode(&bytes),
                Err(CommitSetError::NonCanonical)
            ));
        }
        let empty_bitmap = [0x3a, 0x30, 0, 0, 0, 0, 0, 0];
        let with_empty = [
            &2u64.to_le_bytes()[..],
            &a[8..],
            &1u32.to_le_bytes(),
            &empty_bitmap,
        ]
        .concat();
        assert!(matches!(
            CommitSet::decode(&with_empty),
            Err(CommitSetError::NonCanonical)
        ));
    }

    #[test]
    fn canonicalizes_runs_and_rejects_alternate_valid_encodings() {
        let raw: RoaringTreemap = (1..1000).collect();
        let mut noncanonical = Vec::new();
        raw.serialize_into(&mut noncanonical).unwrap();
        let set = CommitSet(raw);
        let canonical = set.encode().unwrap();
        assert!(canonical.len() < noncanonical.len());
        assert_eq!(CommitSet::decode(&canonical).unwrap(), set);
        assert!(matches!(
            CommitSet::decode(&noncanonical),
            Err(CommitSetError::NonCanonical)
        ));
    }

    #[test]
    fn array_bitmap_and_run_forms_ignore_insertion_order() {
        for values in [
            (0..4096).map(|n| n * 2).collect::<Vec<u64>>(),
            (0..4097).map(|n| n * 2).collect(),
            (0..65536).collect(),
        ] {
            let forward: CommitSet = values.iter().copied().collect();
            let reverse: CommitSet = values.iter().copied().rev().collect();
            let bytes = forward.encode().unwrap();
            assert_eq!(bytes, reverse.encode().unwrap());
            assert_eq!(CommitSet::decode(&bytes).unwrap(), forward);
        }
    }

    proptest! {
        #[test]
        fn codec_and_successor_match_an_ordered_set(values in prop::collection::vec(any::<u64>(), 0..200), target in any::<u64>()) {
            let reference: BTreeSet<_> = values.iter().copied().collect();
            let set: CommitSet = values.into_iter().collect();
            prop_assert_eq!(set.first_after(target), reference.iter().copied().find(|&c| c > target));
            prop_assert_eq!(set.latest(), reference.last().copied());
            if !reference.is_empty() {
                let encoded = set.encode().unwrap();
                prop_assert_eq!(CommitSet::decode(&encoded).unwrap(), set);
            }
        }
    }
}
