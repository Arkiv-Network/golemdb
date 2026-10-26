use golemdb_cells::CellType;
use golemdb_index::INDEX_BRANCH_DOMAIN;
use golemdb_index::{
    BitmapContainer, INDEX_TRIE_PATH_BYTES, Index, IndexTerm, PostingChange, path, tables,
};
use golemdb_merkle::{Keccak256Hasher, LeafRef, RootRef, Trie};
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
/// A root saved as the engine's `Roots` table stores it.
fn save(tx: &mut impl WriteTransaction, key: &[u8], root: &RootRef<INDEX_TRIE_PATH_BYTES>) {
    tx.put(HEAD, key, &root.encode()).unwrap();
}
fn saved(tx: &impl ReadTransaction, key: &[u8]) -> RootRef<INDEX_TRIE_PATH_BYTES> {
    RootRef::decode(&tx.get(HEAD, key).unwrap().unwrap()).unwrap()
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
                0,
                RootRef::Empty,
                [add(b"a", 1), add(b"a", 65536), add(b"b", 2)],
            )
            .unwrap()
            .root;
        save(&mut tx, b"old", &old);
        // The second batch is the next commit, so the old trees stay intact.
        let current = index
            .apply(&mut tx, 1, old, [add(b"a", u64::MAX), add(b"c", 3)])
            .unwrap()
            .root;
        save(&mut tx, b"current", &current);
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
    let root = saved(&read, b"current");
    index.validate_root(&read, root).unwrap();
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

    // Old trie branches remain walkable through the latest read transaction,
    // and keep the hashes of the containers they were built with; the
    // containers themselves are live rows, overwritten in place.
    let container = |value: &[u8], ids: &[u64]| {
        let (hi, _) = path::split(ids[0]);
        let chunk =
            BitmapContainer::from_values(hi, ids.iter().map(|&id| path::split(id).1)).unwrap();
        let term_path = term(value).routing_path(&HASH);
        let mut trie_path = [0; INDEX_TRIE_PATH_BYTES];
        trie_path[..32].copy_from_slice(&term_path);
        trie_path[32..].copy_from_slice(&path::path_bytes(hi).unwrap());
        LeafRef::new(trie_path, chunk.leaf_hash(&term_path, &HASH).unwrap())
    };
    let index_trie =
        Trie::<_, INDEX_TRIE_PATH_BYTES>::new(tables::INDEX_TRIE, INDEX_BRANCH_DOMAIN, &HASH);
    let mut expected = [
        container(b"a", &[1]),
        container(b"a", &[65536]),
        container(b"b", &[2]),
    ];
    expected.sort_by_key(|leaf| leaf.path);
    assert_eq!(
        index_trie
            .walk(&read, saved(&read, b"old"))
            .collect::<golemdb_merkle::Result<Vec<_>>>()
            .unwrap(),
        expected
    );

    if std::env::var("GOLEM_MDBX_TEST_MODE").unwrap() == "uncommitted" {
        let mut tx = db.begin_write().unwrap();
        let changed = index
            .apply(&mut tx, 2, root, [add(b"uncommitted", 42), add(b"a", 7)])
            .unwrap();
        save(&mut tx, b"current", &changed.root);
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
            .apply(&mut tx, 0, RootRef::Empty, [add(b"only", 42)])
            .unwrap()
            .root;
        save(&mut tx, b"current", &root);
        tx.commit().unwrap();
    }
    {
        let db = MdbxStore::open(dir.path()).unwrap();
        let mut tx = db.begin_write().unwrap();
        let root = saved(&tx, b"current");
        index.validate_root(&tx, root).unwrap();
        assert!(matches!(root, RootRef::Leaf(_)));
        let root = index
            .apply(
                &mut tx,
                0,
                root,
                [PostingChange::Remove {
                    term: term(b"only"),
                    record_id: 42,
                }],
            )
            .unwrap()
            .root;
        assert_eq!(root, RootRef::Empty);
        save(&mut tx, b"current", &root);
        tx.commit().unwrap();
    }
    let db = MdbxStore::open(dir.path()).unwrap();
    let tx = db.begin_read().unwrap();
    let root = saved(&tx, b"current");
    assert_eq!(root, RootRef::Empty);
    index.validate_root(&tx, root).unwrap();
}
