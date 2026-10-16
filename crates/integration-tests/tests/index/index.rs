use crate::support::mdbx as fixture;
use golemdb_index::INDEX_BRANCH_DOMAIN;

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
};

use golemdb_cells::{CellNameRef, CellType, CellValue, Width};
use golemdb_index::{
    BitmapContainer, INDEX_TRIE_PATH_BYTES, Index, IndexError, IndexTerm, PostingChange, TermRange,
    tables,
};
use golemdb_merkle::{Hash, HashProvider, Keccak256Hasher, LeafRef, RootRef, Trie};
use golemdb_storage::{MemoryStore, ReadTransaction, Store, WriteTransaction, scan_prefix};
use proptest::prelude::*;

const HASH: Keccak256Hasher = Keccak256Hasher;
fn term(value: u8) -> IndexTerm {
    IndexTerm::new("tag", CellType::Str, &[b'a' + value]).unwrap()
}

/// The `str` terms of `tag` between the values of [`term`] `lower` and
/// `upper`, `None` when the bounds admit none.
fn tag_range(lower: Bound<u8>, upper: Bound<u8>) -> Option<TermRange> {
    let value = |n: u8| CellValue::from_str(&char::from(b'a' + n).to_string());
    let (lower, upper) = (lower.map(value), upper.map(value));
    TermRange::between(
        CellNameRef::raw(b"tag"),
        CellType::Str,
        lower.as_ref().map(CellValue::as_view),
        upper.as_ref().map(CellValue::as_view),
    )
    .unwrap()
}

fn add(t: &IndexTerm, id: u64) -> PostingChange {
    PostingChange::Add {
        term: t.clone(),
        record_id: id,
    }
}
fn remove(t: &IndexTerm, id: u64) -> PostingChange {
    PostingChange::Remove {
        term: t.clone(),
        record_id: id,
    }
}
fn rows(tx: &impl ReadTransaction, table: golemdb_storage::Table) -> Vec<golemdb_storage::Entry> {
    scan_prefix(tx, table, vec![])
        .unwrap()
        .collect::<golemdb_storage::Result<_>>()
        .unwrap()
}
fn root_for(tx: &impl ReadTransaction, t: &IndexTerm) -> Option<Hash> {
    tx.get(tables::INDEX, t.as_bytes())
        .unwrap()
        .map(|b| b.try_into().unwrap())
}

// Compare both commitments to a fresh rebuild, not just flat query results.
fn assert_state(
    tx: &impl ReadTransaction,
    root: RootRef<INDEX_TRIE_PATH_BYTES>,
    expected: &BTreeMap<IndexTerm, BTreeSet<u64>>,
) {
    let index = Index::new(&HASH);
    let rebuilt_db = MemoryStore::new();
    let mut rebuilt_tx = rebuilt_db.begin_write().unwrap();
    let mut rebuilt_root = RootRef::Empty;
    for (term, ids) in expected {
        let bitmap = index.bitmap(tx, term).unwrap().unwrap();
        assert_eq!(bitmap.treemap().iter().collect::<BTreeSet<_>>(), *ids);
        rebuilt_root = index
            .apply(
                &mut rebuilt_tx,
                rebuilt_root,
                ids.iter().map(|id| add(term, *id)),
            )
            .unwrap()
            .root;
        assert_eq!(root_for(tx, term), root_for(&rebuilt_tx, term));
    }
    assert_eq!(root, rebuilt_root);
    assert_eq!(index.reopen(tx, root.hash(&HASH)).unwrap(), root);
    let actual_terms = index
        .terms_with_prefix(tx, vec![])
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect::<Vec<_>>();
    assert_eq!(actual_terms, expected.keys().cloned().collect::<Vec<_>>());
    // Independently verify that each flat row is committed in the top trie.
    let trie =
        Trie::<_, INDEX_TRIE_PATH_BYTES>::new(tables::INDEX_TRIE, INDEX_BRANCH_DOMAIN, &HASH);
    let mut leaves = expected
        .keys()
        .map(|term| {
            let bitmap = root_for(tx, term).unwrap();
            LeafRef {
                path: term.routing_path(&HASH),
                hash: term.leaf_hash(&bitmap, &HASH),
            }
        })
        .collect::<Vec<_>>();
    leaves.sort_by_key(|leaf| leaf.path);
    assert_eq!(
        trie.walk(tx, root)
            .collect::<golemdb_merkle::Result<Vec<_>>>()
            .unwrap(),
        leaves
    );
}

#[test]
fn empty_singleton_branches_and_last_posting_removal() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    let t = term(0);
    assert_eq!(
        index
            .reopen(&tx, RootRef::<INDEX_TRIE_PATH_BYTES>::Empty.hash(&HASH))
            .unwrap(),
        RootRef::Empty
    );
    assert_eq!(index.bitmap(&tx, &t).unwrap(), None);
    let update = index.apply(&mut tx, RootRef::Empty, [add(&t, 0)]).unwrap();
    assert!(matches!(update.root, RootRef::Leaf(_)));
    assert_eq!(update.changed_terms.len(), 1);
    assert_eq!(update.changed_terms[0].before, None);
    assert!(rows(&tx, tables::BITMAP_TRIE).is_empty());
    assert!(rows(&tx, tables::INDEX_TRIE).is_empty());
    let update = index
        .apply(
            &mut tx,
            update.root,
            [
                add(&t, 65535),
                add(&t, 65536),
                add(&t, u64::MAX),
                add(&term(1), 8),
            ],
        )
        .unwrap();
    assert!(matches!(update.root, RootRef::Branch(_)));
    assert_state(
        &tx,
        update.root,
        &BTreeMap::from([
            (t.clone(), BTreeSet::from([0, 65535, 65536, u64::MAX])),
            (term(1), BTreeSet::from([8])),
        ]),
    );
    let update = index
        .apply(
            &mut tx,
            update.root,
            [
                remove(&term(1), 8),
                remove(&t, 0),
                remove(&t, 65535),
                remove(&t, 65536),
            ],
        )
        .unwrap();
    assert_state(
        &tx,
        update.root,
        &BTreeMap::from([(t.clone(), BTreeSet::from([u64::MAX]))]),
    );
    let update = index
        .apply(&mut tx, update.root, [remove(&t, u64::MAX)])
        .unwrap();
    assert_eq!(update.root, RootRef::Empty);
    assert_eq!(update.changed_terms[0].after, None);
    assert!(rows(&tx, tables::INDEX).is_empty());
    assert!(!rows(&tx, tables::BITMAP_CONTAINER).is_empty()); // immutable history retained
}

#[test]
fn duplicate_changes_cancel_and_shared_containers_are_immutable() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    let a = term(0);
    let b = term(1);
    let update = index
        .apply(
            &mut tx,
            RootRef::Empty,
            [add(&a, 1), add(&a, 1), add(&b, 1)],
        )
        .unwrap();
    assert_eq!(rows(&tx, tables::BITMAP_CONTAINER).len(), 1);
    assert_eq!(root_for(&tx, &a), root_for(&tx, &b));
    let before = root_for(&tx, &a);
    let noop = index
        .apply(
            &mut tx,
            update.root,
            [
                remove(&a, 1),
                add(&a, 1),
                add(&a, 2),
                remove(&a, 2),
                remove(&b, 9),
            ],
        )
        .unwrap();
    assert_eq!(noop.root, update.root);
    assert!(noop.changed_terms.is_empty());
    let changed = index
        .apply(&mut tx, noop.root, [add(&a, 2), add(&a, 65536)])
        .unwrap();
    assert_eq!(changed.changed_terms.len(), 1);
    assert_eq!(changed.changed_terms[0].before, before);
    assert_ne!(changed.changed_terms[0].after, before);
    assert_eq!(root_for(&tx, &b), before);
    assert_eq!(
        index
            .bitmap(&tx, &b)
            .unwrap()
            .unwrap()
            .treemap()
            .iter()
            .collect::<Vec<_>>(),
        vec![1]
    );
    assert_state(
        &tx,
        changed.root,
        &BTreeMap::from([(a, BTreeSet::from([1, 2, 65536])), (b, BTreeSet::from([1]))]),
    );
}

#[test]
fn singleton_cache_survives_inserting_an_earlier_chunk() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    let t = term(0);
    let root = index
        .apply(&mut tx, RootRef::Empty, [add(&t, 9 << 16)])
        .unwrap()
        .root;
    let root = index
        .apply(
            &mut tx,
            root,
            [add(&t, 1), add(&t, (9 << 16) + 1), add(&t, 12 << 16)],
        )
        .unwrap()
        .root;
    assert_state(
        &tx,
        root,
        &BTreeMap::from([(t, BTreeSet::from([1, 9 << 16, (9 << 16) + 1, 12 << 16]))]),
    );
}

#[test]
fn snapshots_reopen_and_abort_keep_all_tables_consistent() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let t = term(0);
    let empty = db.begin_read().unwrap();
    let mut tx = db.begin_write().unwrap();
    let root = index
        .apply(&mut tx, RootRef::Empty, [add(&t, 1)])
        .unwrap()
        .root;
    tx.commit().unwrap();
    assert_eq!(index.bitmap(&empty, &t).unwrap(), None);
    let old = db.begin_read().unwrap();
    let singleton = index.reopen(&old, root.hash(&HASH)).unwrap();
    let mut tx = db.begin_write().unwrap();
    let update = index
        .apply(&mut tx, singleton, [add(&t, 65536), add(&term(1), 42)])
        .unwrap();
    tx.put(
        golemdb_storage::Table("Head"),
        b"root",
        &update.root.hash(&HASH),
    )
    .unwrap();
    tx.abort();
    let latest = db.begin_read().unwrap();
    assert_state(
        &latest,
        root,
        &BTreeMap::from([(t.clone(), BTreeSet::from([1]))]),
    );
    assert!(
        latest
            .get(golemdb_storage::Table("Head"), b"root")
            .unwrap()
            .is_none()
    );
    let mut tx = db.begin_write().unwrap();
    let next = index
        .apply(&mut tx, singleton, [add(&t, 65536), add(&term(1), 42)])
        .unwrap()
        .root;
    tx.commit().unwrap();
    assert_state(
        &old,
        root,
        &BTreeMap::from([(t.clone(), BTreeSet::from([1]))]),
    );
    let latest = db.begin_read().unwrap();
    assert_state(
        &latest,
        next,
        &BTreeMap::from([
            (t, BTreeSet::from([1, 65536])),
            (term(1), BTreeSet::from([42])),
        ]),
    );
    assert!(index.reopen(&latest, root.hash(&HASH)).is_err()); // old singleton needs old flat state
}

#[test]
fn typed_range_and_prefix_scans() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    index
        .apply(
            &mut tx,
            RootRef::Empty,
            (0..5).map(|i| add(&term(i), i as u64)),
        )
        .unwrap();
    let found = index
        .terms(
            &tx,
            tag_range(Bound::Included(1), Bound::Excluded(4)).unwrap(),
        )
        .unwrap()
        .collect::<golemdb_index::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        found.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
        vec![term(1), term(2), term(3)]
    );
    let mut prefix = IndexTerm::prefix("tag", CellType::Str).unwrap();
    prefix.push(b'c');
    assert_eq!(
        index
            .terms_with_prefix(&tx, prefix)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .0,
        term(2)
    );
    assert_eq!(tag_range(Bound::Included(3), Bound::Included(0)), None);
    tx.put(tables::INDEX, term(0).as_bytes(), &[0; 31]).unwrap();
    let mut scan = index.terms_with_prefix(&tx, vec![]).unwrap();
    assert!(scan.next().unwrap().is_err());
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
}

#[test]
fn run_scans_stay_in_their_run_and_load_each_bitmap() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let at =
        |name: &str, value: CellValue| IndexTerm::from_str_name(name, value.as_view()).unwrap();
    let run = [0, 5, u32::MAX].map(|n| at("a", CellValue::from_u32(n)));
    let neighbours = [
        at("a", CellValue::from_u64(1)),
        at("a", CellValue::from_str("")),
        at("ab", CellValue::from_u32(1)),
        at("b", CellValue::from_u32(1)),
    ];
    let mut tx = db.begin_write().unwrap();
    let changes = run
        .iter()
        .chain(&neighbours)
        .enumerate()
        .flat_map(|(n, t)| {
            let n = n as u64;
            [add(t, n), add(t, n + (1 << 40))]
        });
    index.apply(&mut tx, RootRef::Empty, changes).unwrap();
    let u32_range = |lower: Bound<u32>, upper: Bound<u32>| {
        let (lower, upper) = (
            lower.map(CellValue::from_u32),
            upper.map(CellValue::from_u32),
        );
        TermRange::between(
            CellNameRef::raw(b"a"),
            CellType::Uint(Width::W4),
            lower.as_ref().map(CellValue::as_view),
            upper.as_ref().map(CellValue::as_view),
        )
        .unwrap()
    };
    let u32_run = |lower: Bound<u32>, upper: Bound<u32>| {
        let range = u32_range(lower, upper).unwrap();
        index
            .terms(&tx, range)
            .unwrap()
            .map(|entry| {
                let (term, root) = entry.unwrap();
                let bitmap = index.bitmap_at(&tx, root).unwrap();
                let expected = index.bitmap(&tx, &term).unwrap().unwrap();
                assert_eq!(bitmap.treemap(), expected.treemap());
                term
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(u32_run(Bound::Unbounded, Bound::Included(u32::MAX)), run);
    assert_eq!(u32_run(Bound::Included(0), Bound::Unbounded), run);
    assert_eq!(
        u32_run(Bound::Excluded(0), Bound::Included(u32::MAX)),
        run[1..]
    );
    // Bounds that admit no value make no range, so no scan.
    for (lower, upper) in [
        (Bound::Included(5), Bound::Excluded(5)),
        (Bound::Excluded(5), Bound::Included(5)),
        (Bound::Excluded(5), Bound::Excluded(5)),
        (Bound::Included(6), Bound::Included(5)),
    ] {
        assert_eq!(u32_range(lower, upper), None, "{lower:?}..{upper:?}");
    }
    // Excluded neighbours with no value between are still a key range: the
    // scan finds no term in it.
    assert!(u32_run(Bound::Excluded(5), Bound::Excluded(6)).is_empty());
}

#[test]
fn prefix_scans_hold_the_strings_that_start_with_the_prefix() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let at =
        |name: &str, value: CellValue| IndexTerm::from_str_name(name, value.as_view()).unwrap();
    // In key order: UTF-8 bytes.
    let strings = ["", "a", "ab", "ab\u{10FFFF}", "ac", "é", "\u{10FFFF}"]
        .map(|value| at("a", CellValue::from_str(value)));
    let neighbours = [
        at("a", CellValue::from_bytes4(*b"abcd")),
        at("ab", CellValue::from_str("ab")),
        at("b", CellValue::from_str("a")),
    ];
    let mut tx = db.begin_write().unwrap();
    let changes = strings
        .iter()
        .chain(&neighbours)
        .enumerate()
        .map(|(n, t)| add(t, n as u64));
    index.apply(&mut tx, RootRef::Empty, changes).unwrap();
    let scan = |prefix: &str| {
        let range = TermRange::with_prefix(CellNameRef::raw(b"a"), prefix).unwrap();
        index
            .terms(&tx, range)
            .unwrap()
            .map(|entry| entry.unwrap().0)
            .collect::<Vec<_>>()
    };
    assert_eq!(scan(""), strings);
    assert_eq!(scan("a"), strings[1..5]);
    assert_eq!(scan("ab"), strings[2..4]);
    assert_eq!(scan("é"), strings[5..6]);
    assert_eq!(scan("\u{10FFFF}"), strings[6..]);
    assert!(scan("b").is_empty());
}

#[test]
fn corrupt_missing_and_mismatched_state_is_not_absence() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    let t = term(0);
    let root = index
        .apply(&mut tx, RootRef::Empty, [add(&t, 1)])
        .unwrap()
        .root;
    assert!(matches!(
        index.apply(&mut tx, RootRef::Empty, [add(&t, 2)]),
        Err(IndexError::RootMismatch)
    ));
    assert!(index.reopen(&tx, [42; 32]).is_err());
    assert!(index.reopen(&tx, HASH.hash(&[])).is_err());
    let bitmap = root_for(&tx, &t).unwrap();
    let original = tx.get(tables::BITMAP_CONTAINER, &bitmap).unwrap().unwrap();
    tx.delete(tables::BITMAP_CONTAINER, &bitmap).unwrap();
    assert!(index.bitmap(&tx, &t).is_err());
    assert!(index.apply(&mut tx, root, [add(&t, 2)]).is_err());
    tx.put(tables::BITMAP_CONTAINER, &bitmap, &[0; 6]).unwrap();
    assert!(matches!(
        index.bitmap(&tx, &t),
        Err(IndexError::Corruption(_))
    ));
    tx.put(tables::BITMAP_CONTAINER, &bitmap, &original)
        .unwrap();
    let root = index
        .apply(&mut tx, root, [add(&t, 65536), add(&term(1), 1)])
        .unwrap()
        .root;
    let hash = root.hash(&HASH);
    tx.delete(tables::INDEX_TRIE, &hash).unwrap();
    assert!(index.reopen(&tx, hash).is_err()); // multiple flat rows cannot be a singleton
    let bitmap = root_for(&tx, &t).unwrap();
    tx.delete(tables::BITMAP_TRIE, &bitmap).unwrap();
    assert!(index.bitmap(&tx, &t).is_err());
}

#[test]
fn immutable_container_conflict_is_rejected_without_overwrite() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    let container = BitmapContainer::from_values(0, [1]).unwrap();
    let hash = container.leaf_hash(&HASH).unwrap();
    tx.put(tables::BITMAP_CONTAINER, &hash, b"conflict")
        .unwrap();
    assert!(matches!(
        index.apply(&mut tx, RootRef::Empty, [add(&term(0), 1)]),
        Err(IndexError::ConflictingContent)
    ));
    assert_eq!(
        tx.get(tables::BITMAP_CONTAINER, &hash).unwrap().unwrap(),
        b"conflict"
    );
    assert!(rows(&tx, tables::INDEX).is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn random_batches_match_sets_and_fresh_rebuilds(batches in prop::collection::vec(prop::collection::vec((0u8..5, 0u8..12, any::<bool>()), 0..14), 0..16)) {
        let (_dir, db) = fixture::db(); let index = Index::new(&HASH);
        let mut tx = db.begin_write().unwrap(); let mut root = RootRef::Empty;
        let mut expected = BTreeMap::<IndexTerm, BTreeSet<u64>>::new();
        for batch in batches {
            let before = expected.keys().map(|t| (t.clone(), root_for(&tx, t))).collect::<BTreeMap<_, _>>();
            let changes = batch.into_iter().map(|(t, id, present)| {
                let t = term(t); let id = ((id as u64 / 3) << 16) | (id as u64 % 3);
                if present { expected.entry(t.clone()).or_default().insert(id); add(&t, id) }
                else { if let Some(ids) = expected.get_mut(&t) { ids.remove(&id); if ids.is_empty() { expected.remove(&t); } } remove(&t, id) }
            }).collect::<Vec<_>>();
            let update = index.apply(&mut tx, root, changes).unwrap(); root = update.root;
            let all_terms = before.keys().chain(expected.keys()).cloned().collect::<BTreeSet<_>>();
            let actual_changes = all_terms.into_iter().filter_map(|term| {
                let old = before.get(&term).copied().flatten(); let new = root_for(&tx, &term);
                (old != new).then_some(golemdb_index::TermRootChange { term, before: old, after: new })
            }).collect::<Vec<_>>();
            prop_assert_eq!(update.changed_terms, actual_changes);
            assert_state(&tx, root, &expected);
            for t in (0..5).map(term) {
                let actual = index.bitmap(&tx, &t).unwrap()
                    .map(|bitmap| bitmap.treemap().iter().collect::<BTreeSet<_>>());
                prop_assert_eq!(actual.as_ref(), expected.get(&t));
            }
        }
    }
}
