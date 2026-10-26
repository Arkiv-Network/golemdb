use std::collections::{BTreeMap, BTreeSet};

use crate::{Bitmap, BitmapContainer, BitmapError, path};
use proptest::prelude::*;
use roaring::RoaringBitmap;

#[test]
fn checked_paths_and_offsets() {
    for id in [0, 65535, 65536, u32::MAX as u64, 1 << 32, u64::MAX] {
        let (hi, lo) = path::split(id);
        assert_eq!(path::join(hi, lo).unwrap(), id);
        assert_eq!(path::path_bytes(hi).unwrap(), hi.to_be_bytes()[2..]);
    }
    assert!(path::join(1 << 48, 0).is_err());
    assert!(path::path_bytes(1 << 48).is_err());
    assert!(BitmapContainer::empty(1 << 48).is_err());
    assert!(matches!(
        BitmapContainer::new(0, [65536].into_iter().collect()),
        Err(BitmapError::OffsetOutOfRange { .. })
    ));
}

#[test]
fn stored_vectors_bind_path_and_canonical_representation() {
    // Portable no-run header: cookie 12346, count 1, key 0/cardinality-1 0,
    // payload offset 16, followed by the little-endian value 42.
    let sparse = vec![
        0, 0, 0, 0, 0, 1, 58, 48, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0, 42, 0,
    ];
    let container = BitmapContainer::from_values(1, [42]).unwrap();
    assert_eq!(container.canonical_bytes().unwrap(), sparse);
    assert_eq!(BitmapContainer::decode(&sparse).unwrap(), container);

    // Portable run header: cookie 12347 (one container), run bitmap 1,
    // key 0/cardinality-1 65535; one run spanning all 65536 offsets.
    let full = vec![
        0, 0, 0, 0, 0, 0, 59, 48, 0, 0, 1, 0, 0, 255, 255, 1, 0, 0, 0, 255, 255,
    ];
    let container = BitmapContainer::new(0, (0..65536).collect()).unwrap();
    assert_eq!(container.canonical_bytes().unwrap(), full);
    assert_eq!(BitmapContainer::decode(&full).unwrap().len(), 65536);

    let changed_path = BitmapContainer::from_values(2, [42])
        .unwrap()
        .canonical_bytes()
        .unwrap();
    assert_ne!(sparse, changed_path);
    assert_eq!(&sparse[6..], &changed_path[6..]);
}

#[test]
fn reject_noncanonical_and_malformed_storage() {
    let mut runs = RoaringBitmap::new();
    runs.insert_range(0..3);
    runs.insert_range(10..12);
    let canonical = BitmapContainer::new(0, runs.clone())
        .unwrap()
        .canonical_bytes()
        .unwrap();
    let mut raw = vec![0; 6];
    runs.serialize_into(&mut raw).unwrap();
    assert_ne!(raw, canonical, "run/array tie must choose array");
    assert!(matches!(
        BitmapContainer::decode(&raw),
        Err(BitmapError::NonCanonicalEncoding)
    ));
    for n in 0..canonical.len() {
        assert!(BitmapContainer::decode(&canonical[..n]).is_err());
    }
    let mut trailing = canonical.clone();
    trailing.push(0);
    assert!(matches!(
        BitmapContainer::decode(&trailing),
        Err(BitmapError::NonCanonicalEncoding)
    ));
    let mut wrong_offset = vec![0; 6];
    RoaringBitmap::from_iter([65536])
        .serialize_into(&mut wrong_offset)
        .unwrap();
    assert!(matches!(
        BitmapContainer::decode(&wrong_offset),
        Err(BitmapError::OffsetOutOfRange { .. })
    ));
    let mut empty = vec![0; 6];
    RoaringBitmap::new().serialize_into(&mut empty).unwrap();
    assert!(matches!(
        BitmapContainer::decode(&empty),
        Err(BitmapError::EmptyContainer)
    ));
}

#[test]
fn container_mutations_and_empty_policy() {
    let mut container = BitmapContainer::empty(1).unwrap();
    assert!(matches!(
        container.canonical_bytes(),
        Err(BitmapError::EmptyContainer)
    ));
    assert!(container.insert(42));
    assert!(!container.insert(42));
    assert_eq!(
        container.iter().collect::<Vec<_>>(),
        [(1 << crate::CHUNK_BITS) + 42]
    );
    let first = container.canonical_bytes().unwrap();
    assert_eq!(first[crate::HI48_BYTES..], container.value_bytes().unwrap());
    assert!(container.remove(42));
    assert!(!container.remove(42));
    assert!(container.is_empty());
}

#[test]
fn canonical_across_build_paths_and_thresholds() {
    for values in [
        vec![0, 1, 2, 10, 11],
        (0..1000).collect(),
        (0..4096).map(|x| x * 2).collect(),
        (0..4097).map(|x| x * 2).collect(),
        (0..65536).collect(),
    ] {
        let ascending: RoaringBitmap = values.iter().copied().collect();
        let reversed: RoaringBitmap = values.iter().rev().copied().collect();
        let mut runs = RoaringBitmap::new();
        runs.insert_range(0..65536);
        for v in 0..65536 {
            if !ascending.contains(v) {
                runs.remove(v);
            }
        }
        let mut optimized = ascending.clone();
        optimized.optimize();
        let mut serialized = Vec::new();
        runs.serialize_into(&mut serialized).unwrap();
        let roundtrip = RoaringBitmap::deserialize_from(serialized.as_slice()).unwrap();
        let expected = BitmapContainer::new(0, ascending.clone())
            .unwrap()
            .canonical_bytes()
            .unwrap();
        for built in [ascending, reversed, runs, optimized, roundtrip] {
            let actual = BitmapContainer::new(0, built)
                .unwrap()
                .canonical_bytes()
                .unwrap();
            assert_eq!(actual, expected);
            assert_eq!(
                BitmapContainer::decode(&actual)
                    .unwrap()
                    .canonical_bytes()
                    .unwrap(),
                expected
            );
        }
        if values.len() == 4096 {
            assert_eq!(expected.len(), 6 + 16 + 4096 * 2);
        }
        if values.len() == 4097 {
            assert_eq!(expected.len(), 6 + 16 + 8192);
        }
    }
}

#[test]
fn materializes_across_roaring_partition_boundaries() {
    let paths = [0, 1, 0xffff, 0x10000, (1 << 48) - 1];
    let containers = paths.map(|p| BitmapContainer::from_values(p, [0, 65535]).unwrap());
    let bitmap = Bitmap::from_containers(containers).unwrap();
    let expected: Vec<_> = paths
        .into_iter()
        .flat_map(|p| [p << 16, (p << 16) | 65535])
        .collect();
    assert_eq!(bitmap.treemap().iter().collect::<Vec<_>>(), expected);
    let other =
        Bitmap::from_containers([BitmapContainer::from_values(0, [0, 42]).unwrap()]).unwrap();
    let intersection = bitmap.treemap() & other.treemap();
    assert_eq!(intersection.iter().collect::<Vec<_>>(), [0]);
    let union = bitmap.into_treemap() | other.into_treemap();
    assert_eq!(union.len(), 11);
}

/// A bitmap of `ids`, assembled from containers as storage would.
fn bitmap_of(ids: impl IntoIterator<Item = u64>) -> Bitmap {
    let mut chunks: BTreeMap<u64, Vec<u16>> = BTreeMap::new();
    for id in ids {
        let (hi, lo) = path::split(id);
        chunks.entry(hi).or_default().push(lo);
    }
    Bitmap::from_containers(
        chunks
            .into_iter()
            .map(|(p, v)| BitmapContainer::from_values(p, v).unwrap()),
    )
    .unwrap()
}

#[test]
fn union_merges_overlapping_and_disjoint_containers() {
    let mut union = bitmap_of([1, 2, 1 << 40]);
    union |= bitmap_of([2, 3, u64::MAX]);
    assert_eq!(union, bitmap_of([1, 2, 3, 1 << 40, u64::MAX]));
}

#[test]
fn union_with_the_empty_bitmap_is_identity() {
    let ids = bitmap_of([7, 1 << 32]);
    let mut left = Bitmap::default();
    left |= ids.clone();
    assert_eq!(left, ids);
    let mut right = ids.clone();
    right |= Bitmap::default();
    assert_eq!(right, ids);
}

#[test]
fn assembly_rejects_ambiguous_input_and_omits_empties() {
    assert!(matches!(
        Bitmap::from_containers([
            BitmapContainer::empty(1).unwrap(),
            BitmapContainer::empty(1).unwrap(),
        ]),
        Err(BitmapError::DuplicatePath { path: 1 })
    ));
    assert!(matches!(
        Bitmap::from_containers([
            BitmapContainer::empty(2).unwrap(),
            BitmapContainer::empty(1).unwrap(),
        ]),
        Err(BitmapError::UnsortedPaths { .. })
    ));
    let empty = Bitmap::from_containers([
        BitmapContainer::empty(0xffff).unwrap(),
        BitmapContainer::empty(0x10000).unwrap(),
    ])
    .unwrap();
    assert_eq!(empty.treemap(), &roaring::RoaringTreemap::new());
}

proptest! {
    #[test]
    fn assembly_matches_record_ids(ids in prop::collection::vec(any::<u64>(), 0..300)) {
        let mut chunks: BTreeMap<u64, Vec<u16>> = BTreeMap::new();
        for id in &ids { let (hi, lo) = path::split(*id); chunks.entry(hi).or_default().push(lo); }
        let bitmap = Bitmap::from_containers(chunks.into_iter().map(|(p,v)| BitmapContainer::from_values(p,v).unwrap())).unwrap();
        let expected: BTreeSet<_> = ids.into_iter().collect();
        prop_assert_eq!(bitmap.treemap().iter().collect::<BTreeSet<_>>(), expected);
    }

    #[test]
    fn union_is_set_union(
        a in prop::collection::vec(any::<u64>(), 0..100),
        b in prop::collection::vec(any::<u64>(), 0..100),
    ) {
        let mut union = bitmap_of(a.iter().copied());
        union |= bitmap_of(b.iter().copied());
        prop_assert_eq!(union, bitmap_of(a.into_iter().chain(b)));
    }

    #[test]
    fn arbitrary_chunk_roundtrip_and_order_independence(path in 0u64..(1 << 48), offsets in prop::collection::vec(any::<u16>(), 1..500)) {
        let forward = BitmapContainer::from_values(path, offsets.iter().copied()).unwrap();
        let reverse = BitmapContainer::from_values(path, offsets.iter().rev().copied()).unwrap();
        let bytes = forward.canonical_bytes().unwrap();
        prop_assert_eq!(reverse.canonical_bytes().unwrap(), bytes.clone());
        prop_assert_eq!(BitmapContainer::decode(&bytes).unwrap(), forward);
    }
}
