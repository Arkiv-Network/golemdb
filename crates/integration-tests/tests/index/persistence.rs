use golemdb_cells::CellType;
use golemdb_index::{BITMAP_BRANCH_DOMAIN, INDEX_BRANCH_DOMAIN};
use golemdb_index::{
    BITMAP_TRIE_PATH_BYTES, BitmapContainer, INDEX_TRIE_PATH_BYTES, Index, IndexTerm,
    PostingChange, tables,
};
use golemdb_merkle::{Hash, Keccak256Hasher, LeafRef, RootRef, Trie};
use golemdb_storage::{ReadTransaction, Store, Table, WriteTransaction};
use golemdb_storage_mdbx::MdbxStore;

const HASH: Keccak256Hasher = Keccak256Hasher;
const HEAD: Table = Table("Head");

fn term(value: &[u8]) -> IndexTerm {
    IndexTerm::new("tag", CellType::Str, value).unwrap()
}
fn add(value: &[u8], record_id: u64) -> PostingChange {
    PostingChange::Add {
        term: term(value),
        record_id,
    }
}
fn hash(tx: &impl ReadTransaction, key: &[u8]) -> Hash {
    tx.get(HEAD, key).unwrap().unwrap().try_into().unwrap()
}

#[test]
fn reopen_in_fresh_process_preserves_head_history_and_discards_uncommitted_writes() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = MdbxStore::open(dir.path()).unwrap();
        let index = Index::new(&HASH);
        let mut tx = db.begin_write().unwrap();
        let old = index
            .apply(
                &mut tx,
                RootRef::Empty,
                [add(b"a", 1), add(b"a", 65536), add(b"b", 2)],
            )
            .unwrap()
            .root;
        tx.put(HEAD, b"old", &old.hash(&HASH)).unwrap();
        let old_bitmap = tx
            .get(tables::INDEX, term(b"a").as_bytes())
            .unwrap()
            .unwrap();
        tx.put(HEAD, b"old_bitmap", &old_bitmap).unwrap();
        let current = index
            .apply(&mut tx, old, [add(b"a", u64::MAX), add(b"c", 3)])
            .unwrap()
            .root;
        tx.put(HEAD, b"current", &current.hash(&HASH)).unwrap();
        tx.commit().unwrap();
    }
    for mode in ["read", "uncommitted", "read"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "persistence::persistence_worker", "--nocapture"])
            .env("GOLEM_MDBX_TEST_PATH", dir.path())
            .env("GOLEM_MDBX_TEST_MODE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn persistence_worker() {
    let Some(path) = std::env::var_os("GOLEM_MDBX_TEST_PATH") else {
        return;
    };
    let db = MdbxStore::open(path).unwrap();
    let index = Index::new(&HASH);
    let read = db.begin_read().unwrap();
    let root = index.reopen(&read, hash(&read, b"current")).unwrap();
    assert_eq!(
        index
            .bitmap(&read, &term(b"a"))
            .unwrap()
            .unwrap()
            .treemap()
            .iter()
            .collect::<Vec<_>>(),
        vec![1, 65536, u64::MAX]
    );
    assert!(
        index
            .bitmap(&read, &term(b"b"))
            .unwrap()
            .unwrap()
            .treemap()
            .contains(2)
    );
    assert!(
        index
            .bitmap(&read, &term(b"c"))
            .unwrap()
            .unwrap()
            .treemap()
            .contains(3)
    );
    assert!(
        index
            .bitmap(&read, &term(b"uncommitted"))
            .unwrap()
            .is_none()
    );

    // Old immutable trees remain walkable through the latest read transaction.
    // This is structural history; reopening old flat term state is engine work.
    let bitmap_trie =
        Trie::<_, BITMAP_TRIE_PATH_BYTES>::new(tables::BITMAP_TRIE, BITMAP_BRANCH_DOMAIN, &HASH);
    let old_bitmap = hash(&read, b"old_bitmap");
    let mut ids = Vec::new();
    for leaf in bitmap_trie.walk(&read, RootRef::Branch(old_bitmap)) {
        let leaf = leaf.unwrap();
        let container = BitmapContainer::decode(
            &read
                .get(tables::BITMAP_CONTAINER, &leaf.hash)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(container.leaf_hash(&HASH).unwrap(), leaf.hash);
        ids.extend(container.iter());
    }
    assert_eq!(ids, vec![1, 65536]);
    let index_trie =
        Trie::<_, INDEX_TRIE_PATH_BYTES>::new(tables::INDEX_TRIE, INDEX_BRANCH_DOMAIN, &HASH);
    let old_b = BitmapContainer::from_values(0, [2])
        .unwrap()
        .leaf_hash(&HASH)
        .unwrap();
    let mut expected = [(term(b"a"), old_bitmap), (term(b"b"), old_b)].map(|(t, b)| LeafRef {
        path: t.routing_path(&HASH),
        hash: t.leaf_hash(&b, &HASH),
    });
    expected.sort_by_key(|leaf| leaf.path);
    assert_eq!(
        index_trie
            .walk(&read, RootRef::Branch(hash(&read, b"old")))
            .collect::<golemdb_merkle::Result<Vec<_>>>()
            .unwrap(),
        expected
    );

    if std::env::var("GOLEM_MDBX_TEST_MODE").unwrap() == "uncommitted" {
        let mut tx = db.begin_write().unwrap();
        let changed = index
            .apply(&mut tx, root, [add(b"uncommitted", 42), add(b"a", 7)])
            .unwrap();
        tx.put(HEAD, b"current", &changed.root.hash(&HASH)).unwrap();
        // Exit without running transaction/database destructors. The next
        // process must recover the last committed head and all four tables.
        std::process::exit(0);
    }
}

#[test]
fn singleton_and_empty_roots_reopen_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::new(&HASH);
    {
        let db = MdbxStore::open(dir.path()).unwrap();
        let mut tx = db.begin_write().unwrap();
        let root = index
            .apply(&mut tx, RootRef::Empty, [add(b"only", 42)])
            .unwrap()
            .root;
        tx.put(HEAD, b"current", &root.hash(&HASH)).unwrap();
        tx.commit().unwrap();
    }
    {
        let db = MdbxStore::open(dir.path()).unwrap();
        let mut tx = db.begin_write().unwrap();
        let root = index.reopen(&tx, hash(&tx, b"current")).unwrap();
        assert!(matches!(root, RootRef::Leaf(_)));
        let root = index
            .apply(
                &mut tx,
                root,
                [PostingChange::Remove {
                    term: term(b"only"),
                    record_id: 42,
                }],
            )
            .unwrap()
            .root;
        assert_eq!(root, RootRef::Empty);
        tx.put(HEAD, b"current", &root.hash(&HASH)).unwrap();
        tx.commit().unwrap();
    }
    let db = MdbxStore::open(dir.path()).unwrap();
    let tx = db.begin_read().unwrap();
    assert_eq!(
        index.reopen(&tx, hash(&tx, b"current")).unwrap(),
        RootRef::Empty
    );
}
