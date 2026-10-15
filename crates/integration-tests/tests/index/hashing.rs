use crate::support::mdbx as fixture;
use golemdb_cells::CellType;
use golemdb_index::{Index, IndexTerm, PostingChange};
use golemdb_merkle::{Blake3Hasher, Hash, HashProvider, Keccak256Hasher, RootRef};
use golemdb_storage::{Store, WriteTransaction};
use golemdb_storage_mdbx::MdbxStore;

// Both statically dispatched providers must propagate through term routing,
// container leaves, both branch domains and reopening from persisted bytes.
fn round_trip(hasher: impl HashProvider) -> Hash {
    let (dir, db) = fixture::db();
    let index = Index::new(&hasher);
    let terms = ["blue", "green", "red"]
        .map(|value| IndexTerm::new("color", CellType::Str, value.as_bytes()).unwrap());
    let ids = [0, 1, 65536, u64::MAX];
    let mut tx = db.begin_write().unwrap();
    let root = index
        .apply(
            &mut tx,
            RootRef::Empty,
            terms.iter().flat_map(|term| {
                ids.map(|record_id| PostingChange::Add {
                    term: term.clone(),
                    record_id,
                })
            }),
        )
        .unwrap()
        .root;
    let root = index
        .apply(
            &mut tx,
            root,
            [PostingChange::Remove {
                term: terms[0].clone(),
                record_id: 65536,
            }],
        )
        .unwrap()
        .root;
    tx.commit().unwrap();
    let hash = root.hash(&hasher);
    drop(db);

    let db = MdbxStore::open(dir.path()).unwrap();
    let read = db.begin_read().unwrap();
    let reopened = index.reopen(&read, hash).unwrap();
    assert_eq!(reopened, root);
    for (i, term) in terms.iter().enumerate() {
        let expected = ids
            .into_iter()
            .filter(|id| i != 0 || *id != 65536)
            .collect::<Vec<_>>();
        assert_eq!(
            index
                .bitmap(&read, term)
                .unwrap()
                .unwrap()
                .treemap()
                .iter()
                .collect::<Vec<_>>(),
            expected
        );
    }
    drop(read);
    let mut tx = db.begin_write().unwrap();
    let update = index
        .apply(
            &mut tx,
            reopened,
            terms.iter().flat_map(|term| {
                ids.map(|record_id| PostingChange::Remove {
                    term: term.clone(),
                    record_id,
                })
            }),
        )
        .unwrap();
    assert_eq!(update.root, RootRef::Empty);
    assert_eq!(update.root.hash(&hasher), hasher.hash(b""));
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(
        index.reopen(&read, hasher.hash(b"")).unwrap(),
        RootRef::Empty
    );
    for term in &terms {
        assert!(index.bitmap(&read, term).unwrap().is_none());
    }
    hash
}

#[test]
fn concrete_hash_algorithms_update_and_reopen_both_index_tiers() {
    assert_ne!(round_trip(Keccak256Hasher), round_trip(Blake3Hasher));
}
