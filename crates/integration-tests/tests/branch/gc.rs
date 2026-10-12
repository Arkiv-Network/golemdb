use std::collections::{BTreeMap, BTreeSet};

use golemdb_branch::{
    BranchError, Branches, SealedCommit, create_genesis_in, read_head_state,
    tables::{self as engine, NODE_REFS as REFS, SUPERBLOCK},
};
use golemdb_cells::{
    CellChange, CellKey, CellNameRef, CellType, CellValue, CellValueRef, Cells, tables as cells,
};
use golemdb_index::{Index, IndexTerm, tables as index};
use golemdb_merkle::{BranchNodeCompact, Hash, Keccak256Hasher, RootRef};
use golemdb_storage::{
    Database, MemoryDatabase, ReadTransaction, Table, WriteTransaction, scan_prefix,
};
use golemdb_storage_mdbx::MdbxDatabase;
use proptest::prelude::*;

const HASH: Keccak256Hasher = Keccak256Hasher;
const TABLES: [Table; 4] = [
    cells::CELL_TRIE,
    index::INDEX_TRIE,
    index::BITMAP_TRIE,
    index::BITMAP_CONTAINER,
];

fn key(id: u64, name: &[u8]) -> CellKey {
    CellKey::new(id, CellNameRef::raw(name))
}

fn value(text: &str, indexed: bool) -> CellValue {
    CellValueRef::new(CellType::Str, text.as_bytes(), indexed)
        .unwrap()
        .into()
}

fn rows(tx: &impl ReadTransaction, table: Table) -> Vec<golemdb_storage::Entry> {
    scan_prefix(tx, table, vec![])
        .unwrap()
        .collect::<golemdb_storage::Result<_>>()
        .unwrap()
}

// Independently enumerate the entire head graph, rather than replaying GC deltas.
// Every physical parent contributes one edge even if the parent has many owners.
fn assert_exact_graph(tx: &impl ReadTransaction, sealed: &SealedCommit) {
    let mut pending = Vec::new();
    if let RootRef::Branch(hash) = sealed.cells.root {
        pending.push((0u8, hash));
    }
    if let RootRef::Branch(hash) = sealed.index.root {
        pending.push((1u8, hash));
    }
    for (_, root) in rows(tx, index::INDEX) {
        let hash: Hash = root.try_into().unwrap();
        let kind = if tx.get(index::BITMAP_CONTAINER, &hash).unwrap().is_some() {
            3
        } else {
            2
        };
        pending.push((kind, hash));
    }
    let mut counts = BTreeMap::<(u8, Hash), u64>::new();
    let mut visited = BTreeSet::new();
    while let Some((kind, hash)) = pending.pop() {
        *counts.entry((kind, hash)).or_default() += 1;
        if !visited.insert((kind, hash)) {
            continue;
        }
        let bytes = tx.get(TABLES[kind as usize], &hash).unwrap().unwrap();
        if kind == 2 {
            let node = BranchNodeCompact::<6>::decode(&bytes).unwrap();
            for slot in 0..16 {
                match node.child(slot) {
                    Some(RootRef::Branch(child)) => pending.push((2, child)),
                    Some(RootRef::Leaf(leaf)) => pending.push((3, leaf.hash)),
                    _ => {}
                }
            }
        } else if kind < 2 {
            let node = BranchNodeCompact::<32>::decode(&bytes).unwrap();
            for slot in 0..16 {
                if let Some(RootRef::Branch(child)) = node.child(slot) {
                    pending.push((kind, child));
                }
            }
        }
    }
    for (kind, table) in TABLES.iter().enumerate() {
        assert_eq!(
            rows(tx, *table)
                .into_iter()
                .map(|(k, _)| k)
                .collect::<BTreeSet<_>>(),
            visited
                .iter()
                .filter(|(k, _)| *k == kind as u8)
                .map(|(_, h)| h.to_vec())
                .collect::<BTreeSet<_>>()
        );
    }
    let expected = counts
        .into_iter()
        .map(|((kind, hash), count)| {
            (
                [vec![kind], hash.to_vec()].concat(),
                count.to_be_bytes().to_vec(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(rows(tx, REFS), expected);
}

fn verify_rebuild(db: &impl Database, sealed: &SealedCommit) {
    let tx = db.begin_read().unwrap();
    let values = rows(&tx, cells::CELL)
        .into_iter()
        .map(|(k, v)| (CellKey::decode(&k).unwrap(), CellValue::parse(v).unwrap()))
        .collect::<Vec<_>>();
    let expected = MemoryDatabase::new();
    super::seal::seed(&expected, &HASH, sealed.commit_id, &values);
    let reference = expected.begin_read().unwrap();
    assert_eq!(
        tx.get(SUPERBLOCK, b"head").unwrap(),
        reference.get(SUPERBLOCK, b"head").unwrap()
    );
    assert_eq!(rows(&tx, index::INDEX), rows(&reference, index::INDEX));
    for term in Index::new(&HASH)
        .terms_with_prefix(&reference, vec![])
        .unwrap()
    {
        let (term, _) = term.unwrap();
        assert_eq!(
            Index::new(&HASH).bitmap(&tx, &term).unwrap(),
            Index::new(&HASH).bitmap(&reference, &term).unwrap()
        );
    }
    assert_exact_graph(&tx, sealed);
}

fn shared(
    branches: &Branches<MdbxDatabase, Keccak256Hasher>,
    text: &str,
) -> std::sync::Arc<SealedCommit> {
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            for id in [64, 65536, 131072] {
                for name in [b"a", b"b"] {
                    cells.put(key(id, name), value(text, true));
                }
            }
            Ok::<_, ()>(())
        })
        .unwrap();
    let sealed = branches.seal(branch).unwrap();
    branches.commit(branch).unwrap();
    verify_rebuild(branches.database(), &sealed);
    sealed
}

#[test]
fn mdbx_readers_keep_old_graph_and_counts_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = MdbxDatabase::open(dir.path()).unwrap();
        let initial = [64, 65536, 131072]
            .into_iter()
            .flat_map(|id| {
                [b"a", b"b"]
                    .into_iter()
                    .map(move |name| (key(id, name), value("old", true)))
            })
            .collect::<Vec<_>>();
        super::seal::seed(&db, &HASH, 0, &initial);
        let branches = Branches::new(db.clone(), HASH).unwrap();
        let first = shared(&branches, "old");
        let snapshot = db.begin_read().unwrap();
        let old_rows = TABLES.map(|t| rows(&snapshot, t));
        let second = shared(&branches, "new");
        let latest = db.begin_read().unwrap();
        assert!(
            latest
                .get(cells::CELL_TRIE, &first.state_root)
                .unwrap()
                .is_none()
        );
        assert!(
            latest
                .get(index::INDEX_TRIE, &first.index_root)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            Cells::new(&HASH)
                .reopen(&snapshot, first.state_root)
                .unwrap(),
            first.cells.root
        );
        assert_eq!(
            Index::new(&HASH)
                .reopen(&snapshot, first.index_root)
                .unwrap(),
            first.index.root
        );
        let term = IndexTerm::new("a", CellType::Str, b"old").unwrap();
        assert_eq!(
            Index::new(&HASH)
                .bitmap(&snapshot, &term)
                .unwrap()
                .unwrap()
                .treemap()
                .iter()
                .collect::<Vec<_>>(),
            vec![64, 65536, 131072]
        );
        assert_eq!(Index::new(&HASH).bitmap(&latest, &term).unwrap(), None);
        assert_eq!(TABLES.map(|t| rows(&snapshot, t)), old_rows);
        assert_exact_graph(&latest, &second);
    }
    let db = MdbxDatabase::open(dir.path()).unwrap();
    let branches = Branches::new(db.clone(), HASH).unwrap();
    shared(&branches, "after-restart");
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            for id in [64, 65536, 131072] {
                for name in [b"a", b"b"] {
                    cells.delete(key(id, name));
                }
            }
            Ok::<_, ()>(())
        })
        .unwrap();
    let sealed = branches.seal(branch).unwrap();
    branches.commit(branch).unwrap();
    verify_rebuild(&db, &sealed);
    let tx = db.begin_read().unwrap();
    for table in [
        index::INDEX,
        index::INDEX_TRIE,
        index::BITMAP_TRIE,
        index::BITMAP_CONTAINER,
    ] {
        assert!(rows(&tx, table).is_empty());
    }
}

#[test]
fn mdbx_rejects_legacy_without_reclaiming_any_rows_and_retries_failed_creation() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxDatabase::open(dir.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    let empty = golemdb_merkle::HashProvider::hash(&HASH, &[]);
    tx.put(
        SUPERBLOCK,
        b"head",
        &[42u64.to_be_bytes().as_slice(), &empty, &empty].concat(),
    )
    .unwrap();
    tx.put(cells::CELL_TRIE, b"legacy-key", b"historical-node")
        .unwrap();
    tx.commit().unwrap();
    let old = db.begin_read().unwrap();
    assert!(matches!(
        Branches::new(db.clone(), HASH),
        Err(golemdb_branch::BranchError::UnsupportedDatabaseFormat)
    ));
    assert!(matches!(
        Branches::create(db.clone(), HASH, []),
        Err(golemdb_branch::BranchError::DatabaseNotEmpty(_))
    ));
    let latest = db.begin_read().unwrap();
    for table in [SUPERBLOCK, cells::CELL_TRIE, REFS] {
        assert_eq!(rows(&old, table), rows(&latest, table));
    }
    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = MdbxDatabase::open(fresh_dir.path()).unwrap();
    let bad = golemdb_cells::CellChange::Put {
        key: key(64, b"\xff"),
        value: value("invalid-attribute", true),
    };
    assert!(Branches::create(fresh.clone(), HASH, [bad]).is_err());
    let tx = fresh.begin_read().unwrap();
    for table in [SUPERBLOCK, cells::CELL, cells::CELL_TRIE, REFS] {
        assert!(rows(&tx, table).is_empty());
    }
    drop(tx);
    let branches = Branches::create(fresh.clone(), HASH, []).unwrap();
    shared(&branches, "first-commit");
}

// A host commits genesis with its own rows; it adds Superblock identifiers
// after the engine's, since genesis requires empty engine tables.
fn host_genesis(db: impl Database + Clone) {
    const HOST: Table = Table("Host");
    let snapshot = || {
        let tx = db.begin_read().unwrap();
        engine::ENGINE
            .into_iter()
            .chain([HOST])
            .map(|table| rows(&tx, table))
            .collect::<Vec<_>>()
    };
    let cells = || {
        [64, 65536].into_iter().map(|id| CellChange::Put {
            key: key(id, b"a"),
            value: value("genesis", true),
        })
    };
    let mut tx = db.begin_write().unwrap();
    tx.put(SUPERBLOCK, b"hash_fn", &1u16.to_be_bytes()).unwrap();
    assert!(matches!(
        create_genesis_in(&mut tx, &HASH, cells()),
        Err(BranchError::DatabaseNotEmpty(table)) if table == SUPERBLOCK
    ));
    tx.abort();
    let empty = snapshot();
    for durable in [false, true] {
        let mut tx = db.begin_write().unwrap();
        tx.put(HOST, b"before", b"genesis").unwrap();
        let head = create_genesis_in(&mut tx, &HASH, cells()).unwrap();
        tx.put(SUPERBLOCK, b"hash_fn", &1u16.to_be_bytes()).unwrap();
        tx.put(HOST, b"after", b"genesis").unwrap();
        if durable {
            tx.commit().unwrap();
            assert_eq!(read_head_state(&db.begin_read().unwrap()).unwrap(), head);
        } else {
            tx.abort();
            assert_eq!(snapshot(), empty);
        }
    }
    let branches = Branches::new(db.clone(), HASH).unwrap();
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(64, b"a"), value("next", true));
            cells.delete(key(65536, b"a"));
            Ok::<_, ()>(())
        })
        .unwrap();
    let sealed = branches.seal(branch).unwrap();
    assert_eq!(branches.commit(branch).unwrap(), 1);
    verify_rebuild(&db, &sealed);
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.get(HOST, b"after").unwrap(), Some(b"genesis".to_vec()));
}

#[test]
fn memory_host_genesis_is_atomic_with_host_rows() {
    host_genesis(MemoryDatabase::new());
}

#[test]
fn mdbx_host_genesis_is_atomic_with_host_rows() {
    let dir = tempfile::tempdir().unwrap();
    host_genesis(MdbxDatabase::open(dir.path()).unwrap());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn churn_matches_rebuild_and_has_no_orphans(
        batches in prop::collection::vec(prop::collection::vec(
            (0u8..12, any::<bool>(), 0u8..5, 0u8..4), 0..8), 1..25)
    ) {
        let db = MemoryDatabase::new();
        super::seal::seed(&db, &HASH, 0, &[]);
        let branches = Branches::new(db.clone(), HASH).unwrap();
        for batch in batches {
            let branch = branches.begin().unwrap();
            branches.write(branch, |cells| {
                for &(id, name, text, action) in &batch {
                    let id = (u64::from(id % 3) << 16) + 64 + u64::from(id / 3);
                    let key = key(id, if name { b"a" } else { b"b" });
                    if action == 0 {
                        cells.delete(key);
                    } else {
                        cells.put(key, value(&format!("v{text}"), action > 1));
                    }
                }
                Ok::<_, ()>(())
            }).unwrap();
            let sealed = branches.seal(branch).unwrap();
            branches.commit(branch).unwrap();
            verify_rebuild(&db, &sealed);
        }
    }
}
