use std::collections::{BTreeMap, BTreeSet};

use golemdb_cells::tables::CELL_TRIE;
use golemdb_cells::{CellType, CellValue, Width};
use golemdb_immutable_data::{
    ImmutableDataColumn, ImmutableDataCompression, ImmutableDataConfig, ImmutableDataRow,
    ImmutableDataSegment, ImmutableDataStorage,
};
use golemdb_index::tables::{BITMAP_CONTAINER, BITMAP_TRIE, INDEX_TRIE};
use golemdb_index::{BITMAP_BRANCH_DOMAIN, BITMAP_TRIE_PATH_BYTES, INDEX_BRANCH_DOMAIN};
use golemdb_merkle::Keccak256Hasher;
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction, scan_prefix};

use crate::publication_tests::{bitmap_roots, key, reachable_rows, seed, value};
use golemdb_pruning::{MAX_WAIT_COMMITS, PruningConfig, STALE};

use crate::{Branches, ROOTS};

/// The versioned tables, in the order of their codes in `Stale` keys.
fn versioned() -> Vec<Table> {
    [
        &golemdb_cells::tables::VERSIONED[..],
        &golemdb_index::tables::VERSIONED,
    ]
    .concat()
}

type Rows = BTreeSet<(Table, Vec<u8>)>;

/// Every row the head's tries reach, containers included.
fn reachable(db: &impl Store) -> Rows {
    let tx = db.begin_read().unwrap();
    let head = crate::read_head_state(&tx).unwrap();
    let roots = crate::head_roots(&tx, &head, &Keccak256Hasher).unwrap();
    let bitmaps = bitmap_roots(db);
    let mut rows = Rows::new();
    let tagged = |table, keys: BTreeSet<Vec<u8>>| keys.into_iter().map(move |key| (table, key));
    let cell_domain = golemdb_cells::CELL_BRANCH_DOMAIN;
    rows.extend(tagged(
        CELL_TRIE.table(),
        reachable_rows(&tx, CELL_TRIE, cell_domain, &[([], roots.state)]),
    ));
    rows.extend(tagged(
        INDEX_TRIE.table(),
        reachable_rows(&tx, INDEX_TRIE, INDEX_BRANCH_DOMAIN, &[([], roots.index)]),
    ));
    rows.extend(tagged(
        BITMAP_TRIE.table(),
        reachable_rows(&tx, BITMAP_TRIE, BITMAP_BRANCH_DOMAIN, &bitmaps),
    ));
    for (term, root) in &bitmaps {
        let trie = golemdb_merkle::PayloadTrie::<_, BITMAP_TRIE_PATH_BYTES, _>::namespaced(
            BITMAP_TRIE,
            BITMAP_CONTAINER,
            *term,
            BITMAP_BRANCH_DOMAIN,
            &Keccak256Hasher,
        );
        for leaf in trie.walk(&tx, *root) {
            let leaf = leaf.unwrap();
            rows.insert((
                BITMAP_CONTAINER.table(),
                trie.leaf_key(leaf.commit, &leaf.path),
            ));
        }
    }
    rows
}

fn keys(tx: &impl ReadTransaction, table: Table) -> Vec<Vec<u8>> {
    scan_prefix(tx, table, vec![])
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect()
}

/// Immutable storage with one segment, so commits take the `publish` path.
fn immutable_data() -> ImmutableDataStorage {
    let segment = ImmutableDataSegment {
        name: "events".into(),
        columns: vec![ImmutableDataColumn::new("n").with_type(CellType::Uint(Width::W8))],
        compression: ImmutableDataCompression::None,
    };
    let config = ImmutableDataConfig::new(4, vec![segment]);
    ImmutableDataStorage::in_memory(config, [7; 32]).unwrap()
}

/// A branch over the head with `ops`: `(record, choice)`, choice 0 deletes the
/// record's tag and any other value sets it.
fn write<S: Store>(branches: &Branches<S, Keccak256Hasher>, ops: &[(u64, u64)]) -> crate::BranchId {
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            for &(record, choice) in ops {
                let id = golemdb_index::path::join(record % 3, 64 + record as u16).unwrap();
                match choice {
                    0 => cells.delete(key(id, b"tag")),
                    _ => cells.put(key(id, b"tag"), value(&format!("t{choice}"), true)),
                }
            }
            Ok::<_, ()>(())
        })
        .unwrap();
    branch
}

/// One property-test run's settings.
#[derive(Clone, Copy)]
struct Case {
    window: u64,
    /// The window after the reopen halfway, if it changes.
    shrunk: Option<u64>,
    seed: u64,
    immutable: bool,
}

impl Case {
    fn new(window: u64, seed: u64) -> Self {
        Self {
            window,
            shrunk: None,
            seed,
            immutable: false,
        }
    }
}

/// Commits per run: enough for listed rows to wait out [`MAX_WAIT_COMMITS`].
const ROUNDS: u64 = 120;

/// Random commits of creates, updates and deletes over indexed cells spread
/// across several containers. These runs never fill a batch, so due rows go
/// once they have waited [`MAX_WAIT_COMMITS`]. After every commit: the window's
/// rows are all stored, every other stored row is listed in `Stale`, nothing
/// listed is already gone or has waited longer, `Roots` holds exactly the
/// window, and the head matches a run without pruning. Halfway, the store goes
/// through `reopen` and a new manager takes over, so `Stale` must carry the
/// sweep's progress; with a shrunk window, `Roots` follows at once.
fn run<S: Store + Clone>(db: S, case: Case, reopen: impl Fn(S) -> S) {
    let mut window = case.window;
    let mut db = db;
    seed(&db, &Keccak256Hasher, 0, &[]);
    let open = |db: &S, window| {
        let storage = case.immutable.then(immutable_data);
        let config = Some(PruningConfig::new(window));
        Branches::new(db.clone(), Keccak256Hasher, storage, config).unwrap()
    };
    let mut branches = open(&db, window);
    let plain_db = MemoryStore::new();
    seed(&plain_db, &Keccak256Hasher, 0, &[]);
    let plain = Branches::new(plain_db.clone(), Keccak256Hasher, None, None).unwrap();
    let mut live = BTreeMap::from([(0, reachable(&db))]);
    let mut state = case.seed;
    let mut random = move |bound: u64| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % bound
    };
    for round in 0..ROUNDS {
        // In-memory immutable storage would not survive a reopen.
        if round == ROUNDS / 2 && !case.immutable {
            drop(branches);
            db = reopen(db);
            window = case.shrunk.unwrap_or(window);
            branches = open(&db, window);
        }
        let ops: Vec<_> = (0..1 + random(12))
            .map(|_| (random(24), random(6)))
            .collect();
        let branch = write(&branches, &ops);
        if case.immutable {
            let row = ImmutableDataRow::new(vec![CellValue::from_u64(round)]);
            branches
                .immutable_data_append(branch, "events", None, row)
                .unwrap();
        }
        let head = branches.commit(branch).unwrap().get();
        assert_eq!(head, round + 1);
        plain.commit(write(&plain, &ops)).unwrap();
        assert_eq!(
            crate::read_head_state(&db.begin_read().unwrap()).unwrap(),
            crate::read_head_state(&plain_db.begin_read().unwrap()).unwrap(),
            "pruning changed the head"
        );
        live.insert(head, reachable(&db));

        let earliest = head.saturating_sub(window);
        let required: Rows = (earliest..=head).flat_map(|c| live[&c].clone()).collect();
        let tx = db.begin_read().unwrap();
        let stored: Rows = versioned()
            .iter()
            .flat_map(|&table| keys(&tx, table).into_iter().map(move |key| (table, key)))
            .collect();
        let stale_keys = keys(&tx, STALE);
        // Each key is `commit ‖ table ‖ key`.
        let stale: Rows = (stale_keys.iter())
            .map(|key| (versioned()[key[8] as usize], key[9..].to_vec()))
            .collect();
        assert!(
            required.is_subset(&stored),
            "a row in the window was deleted"
        );
        assert!(stale.is_subset(&stored), "a listed row is already gone");
        let unlisted: Rows = stored.difference(&required).cloned().collect();
        assert!(unlisted.is_subset(&stale), "a dead row is not listed");
        for key in &stale_keys {
            let commit = u64::from_be_bytes(key[..8].try_into().unwrap());
            assert!(
                commit + window + MAX_WAIT_COMMITS > head,
                "a row superseded at {commit} is still listed at head {head}"
            );
        }
        let roots: Vec<u64> = keys(&tx, ROOTS)
            .iter()
            .map(|key| u64::from_be_bytes(key[..].try_into().unwrap()))
            .collect();
        assert_eq!(roots, (earliest..=head).collect::<Vec<_>>());
    }
}

#[test]
fn gc_keeps_the_window_and_lists_every_dead_row() {
    for window in [0, 1, 2, 3, 5] {
        for seed_value in [1, 2, 3] {
            run(MemoryStore::new(), Case::new(window, seed_value), |db| db);
        }
    }
}

/// Reopening with a smaller window drops `Roots` at once, and the rows it
/// makes due go with the next batch.
#[test]
fn gc_follows_a_shrunk_window() {
    for (window, shrunk) in [(100, 2), (80, 0)] {
        let case = Case {
            shrunk: Some(shrunk),
            ..Case::new(window, 4)
        };
        run(MemoryStore::new(), case, |db| db);
    }
}

#[test]
fn gc_runs_on_the_immutable_publish_path() {
    for window in [0, 3] {
        let case = Case {
            immutable: true,
            ..Case::new(window, 1)
        };
        run(MemoryStore::new(), case, |db| db);
    }
}

/// The same checks on MDBX, with the store closed and reopened halfway through.
#[test]
fn gc_survives_a_reopen_on_mdbx() {
    for (window, shrunk) in [(0, None), (3, None), (100, Some(2))] {
        let dir = tempfile::tempdir().unwrap();
        let open = || golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
        let case = Case {
            shrunk,
            ..Case::new(window, 7)
        };
        run(open(), case, |db| {
            drop(db);
            open()
        });
    }
}

#[test]
fn malformed_stale_rows_are_dropped_without_failing_the_commit() {
    // Too short for a commit, no table code, and an unknown table code.
    let unknown_table = [&[0; 8][..], &[9], b"x"].concat();
    for row in [&[0; 5][..], &[0; 8], &unknown_table] {
        let db = MemoryStore::new();
        seed(&db, &Keccak256Hasher, 0, &[]);
        let branches = Branches::new(
            db.clone(),
            Keccak256Hasher,
            None,
            Some(PruningConfig::new(0)),
        )
        .unwrap();
        // Listed by commit 0, the row sorts before any later commit's and has
        // waited long enough once the head passes the wait.
        let mut tx = db.begin_write().unwrap();
        tx.put(STALE, row, &[]).unwrap();
        tx.commit().unwrap();
        for round in 0..=MAX_WAIT_COMMITS {
            let branch = branches.begin().unwrap();
            branches
                .write(branch, |cells| {
                    cells.put(key(64, b"tag"), value(&format!("t{round}"), true));
                    Ok::<_, ()>(())
                })
                .unwrap();
            branches.commit(branch).unwrap();
        }
        let tx = db.begin_read().unwrap();
        assert!(tx.get(STALE, row).unwrap().is_none());
    }
}
