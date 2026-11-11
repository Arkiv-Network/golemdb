use std::cell::Cell;
use std::collections::BTreeSet;
use std::ops::Bound;
use std::os::unix::fs::PermissionsExt;

use golemdb_branch::{Branches, CommitId, ROOTS, Roots, init_head, read_roots};
use golemdb_cells::{
    CELL_BRANCH_DOMAIN, CellKey, CellNameRef, CellType, CellValue, CellValueRef, reserved, system,
    tables::{CELL, CELL_TRIE},
};
use golemdb_compact::{PruneError, PruneOptions, Report, prune};
use golemdb_index::{
    INDEX_BRANCH_DOMAIN,
    tables::{BITMAP_CONTAINER, BITMAP_TRIE, INDEX_TRIE},
};
use golemdb_merkle::{Keccak256Hasher, RootRef, Trie};
use golemdb_pruning::{PruningConfig, STALE};
use golemdb_storage::{MemoryStore, StorageError, Store, Table, WriteTransaction, scan};
use golemdb_storage_mdbx::MdbxStore;

const SUPERBLOCK: Table = Table("Superblock");
const OTHER: Table = Table("Other");
const LIVE: [Table; 3] = [SUPERBLOCK, CELL, golemdb_index::tables::INDEX];
const ROUNDS: u64 = 80;

fn genesis(db: &impl Store) {
    let mut tx = db.begin_write().unwrap();
    tx.put(SUPERBLOCK, b"hash_fn", &1u16.to_be_bytes()).unwrap();
    let empty = Roots {
        state: RootRef::Empty,
        index: RootRef::Empty,
    };
    init_head(&mut tx, &empty, &Keccak256Hasher).unwrap();
    tx.commit().unwrap();
}

/// `ROUNDS` commits of random tag puts and deletes, each with the pruning
/// `phase` gives its round.
fn history<S: Store + Clone>(db: &S, seed: u64, phase: impl Fn(u64) -> Option<u64>) {
    genesis(db);
    let mut state = seed;
    let mut random = move |bound: u64| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % bound
    };
    for round in 0..ROUNDS {
        let pruning = phase(round).map(PruningConfig::new);
        let branches = Branches::new(db.clone(), Keccak256Hasher, None, pruning).unwrap();
        let branch = branches.begin().unwrap();
        let ops: Vec<_> = (0..1 + random(12))
            .map(|_| (random(24), random(6)))
            .collect();
        branches
            .write(branch, |cells| {
                for (record, choice) in ops {
                    let id = (record % 3) << 16 | (64 + record);
                    let key = CellKey::new(id, CellNameRef::parse_user(b"tag", 64).unwrap());
                    match choice {
                        0 => cells.delete(key),
                        _ => {
                            let text = format!("t{choice}");
                            let value = CellValueRef::new(CellType::Str, text.as_bytes(), true);
                            cells.put(key, CellValue::from(value.unwrap()))
                        }
                    }
                }
                Ok::<_, ()>(())
            })
            .unwrap();
        branches.commit(branch).unwrap();
    }
    set_min_retention(db, 0);
}

/// Beside the cell trie: [`prune`] reads the cell alone.
fn set_min_retention(db: &impl Store, min: u64) {
    let key = CellKey::new(system::PARAMS.id, reserved::MIN_RETENTION);
    let mut tx = db.begin_write().unwrap();
    let min = CellValue::from_u64(min);
    tx.put(CELL, &key.encode(), &min.as_view().encode())
        .unwrap();
    tx.commit().unwrap();
}

type Rows = BTreeSet<(Table, Vec<u8>, Vec<u8>)>;

fn rows(db: &impl Store, tables: &[Table]) -> Rows {
    let tx = db.begin_read().unwrap();
    let mut rows = Rows::new();
    for &table in tables {
        for row in scan(&tx, table, Bound::Unbounded, Bound::Unbounded).unwrap() {
            let (key, value) = row.unwrap();
            rows.insert((table, key, value));
        }
    }
    rows
}

fn stale(db: &impl Store) -> BTreeSet<(u64, u8, Vec<u8>)> {
    rows(db, &[STALE])
        .into_iter()
        .map(|(_, key, _)| {
            let commit = u64::from_be_bytes(key[..8].try_into().unwrap());
            (commit, key[8], key[9..].to_vec())
        })
        .collect()
}

/// Small batches, so every step crosses several.
fn options(window: u64) -> PruneOptions {
    PruneOptions::new(PruningConfig::new(window)).batch_rows(7)
}

/// Indexed by `Stale` table code.
fn tries() -> [Table; 4] {
    [
        CELL_TRIE.table(),
        INDEX_TRIE.table(),
        BITMAP_TRIE.table(),
        BITMAP_CONTAINER.table(),
    ]
}

/// `store` ends as a store pruned with `window` from genesis once swept, less
/// earlier bitmap roots' rows; other tables are untouched.
fn assert_matches_pruned(store: &impl Store, window: u64, seed: u64) -> Report {
    let pruned = MemoryStore::new();
    history(&pruned, seed, |_| Some(window));
    let untouched = [&LIVE[..], &[OTHER]].concat();
    let before = rows(store, &untouched);
    let tries_before = rows(store, &tries()).len() as u64;
    let report = prune(store, &MemoryStore::new(), &options(window)).unwrap();

    let cutoff = ROUNDS.saturating_sub(window);
    let pruned_stale = stale(&pruned);
    let gone: BTreeSet<_> = (pruned_stale.iter())
        .filter(|(commit, code, _)| *commit <= cutoff || *code >= 2)
        .map(|(_, code, key)| (tries()[*code as usize], key.clone()))
        .collect();
    let kept: BTreeSet<_> = (rows(&pruned, &tries()).into_iter())
        .filter(|(table, key, _)| !gone.contains(&(*table, key.clone())))
        .collect();
    let stored = rows(store, &tries());
    assert_eq!(stored, kept, "trie rows");
    let pending: BTreeSet<_> = (pruned_stale.into_iter())
        .filter(|(commit, code, _)| *commit > cutoff && *code < 2)
        .collect();
    let listed = stale(store);
    assert!(listed.iter().all(|(commit, _, _)| *commit > cutoff));
    // Listings of deleted bitmap rows may remain.
    let listed: BTreeSet<_> = (listed.into_iter())
        .filter(|(_, code, _)| *code < 2)
        .collect();
    assert_eq!(listed, pending, "Stale");
    assert_eq!(rows(store, &[ROOTS]), rows(&pruned, &[ROOTS]), "Roots");
    assert_eq!(rows(store, &untouched), before, "other tables");
    assert_eq!(
        report,
        Report {
            head: ROUNDS,
            earliest: cutoff,
            kept: kept.len() as u64,
            listed: pending.len() as u64,
            deleted: tries_before - kept.len() as u64,
        }
    );
    report
}

fn with_other_table(store: &impl Store) {
    let mut tx = store.begin_write().unwrap();
    tx.put(OTHER, b"key", b"value").unwrap();
    tx.commit().unwrap();
}

#[test]
fn pruning_an_unpruned_store_matches_pruning_from_genesis() {
    for window in [0, 1, 3, 20, ROUNDS] {
        for seed in [1, 2] {
            let store = MemoryStore::new();
            history(&store, seed, |_| None);
            with_other_table(&store);
            assert_matches_pruned(&store, window, seed);
            assert_eq!(stale(&store).len() as u64, {
                let pruned = MemoryStore::new();
                history(&pruned, seed, |_| Some(window));
                let cutoff = ROUNDS.saturating_sub(window);
                (stale(&pruned).iter())
                    .filter(|(commit, code, _)| *commit > cutoff && *code < 2)
                    .count() as u64
            });
        }
    }
}

#[test]
fn pruning_deletes_rows_superseded_while_pruning_was_off() {
    for window in [2, 10] {
        let store = MemoryStore::new();
        history(&store, 3, |round| {
            (!(20..50).contains(&round)).then_some(window)
        });
        with_other_table(&store);
        let listed_before = stale(&store).len() as u64;
        let report = assert_matches_pruned(&store, window, 3);
        assert!(report.deleted > listed_before);
    }
}

#[test]
fn pruning_again_changes_nothing() {
    let store = MemoryStore::new();
    history(&store, 4, |_| None);
    prune(&store, &MemoryStore::new(), &options(5)).unwrap();
    let all = [&LIVE[..], &tries(), &[STALE, ROOTS]].concat();
    let before = rows(&store, &all);
    let report = prune(&store, &MemoryStore::new(), &options(5)).unwrap();
    assert_eq!(report.deleted, 0);
    assert_eq!(rows(&store, &all), before);
}

#[test]
fn a_bigger_window_keeps_the_roots_the_store_holds() {
    let store = MemoryStore::new();
    history(&store, 8, |_| Some(3));
    let report = prune(&store, &MemoryStore::new(), &options(10)).unwrap();
    assert_eq!(report.earliest, ROUNDS - 3);
    assert_eq!(rows(&store, &[ROOTS]).len(), 4);
    assert_eq!(assert_matches_pruned(&store, 3, 8).deleted, 0);
}

/// Fails writes after `writes` transactions.
struct Interrupted<'a> {
    store: &'a MemoryStore,
    writes: Cell<usize>,
}

impl Store for Interrupted<'_> {
    type Read<'a>
        = <MemoryStore as Store>::Read<'a>
    where
        Self: 'a;
    type Write<'a>
        = <MemoryStore as Store>::Write<'a>
    where
        Self: 'a;
    fn max_key_size(&self) -> usize {
        self.store.max_key_size()
    }
    fn max_value_size(&self) -> usize {
        self.store.max_value_size()
    }
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        self.store.begin_read()
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        let Some(writes) = self.writes.get().checked_sub(1) else {
            return Err(StorageError::Backend(
                std::io::Error::other("interrupted").into(),
            ));
        };
        self.writes.set(writes);
        self.store.begin_write()
    }
}

fn assert_roots_readable(store: &impl Store) {
    let tx = store.begin_read().unwrap();
    let cells = Trie::new(CELL_TRIE, CELL_BRANCH_DOMAIN, &Keccak256Hasher);
    let index = Trie::new(INDEX_TRIE, INDEX_BRANCH_DOMAIN, &Keccak256Hasher);
    for (key, _) in rows(store, &[ROOTS]).into_iter().map(|(_, k, v)| (k, v)) {
        let commit = u64::from_be_bytes(key.try_into().unwrap());
        let roots = read_roots(&tx, CommitId::new(commit)).unwrap().unwrap();
        for leaf in cells.walk(&tx, roots.state) {
            leaf.unwrap();
        }
        for leaf in index.walk(&tx, roots.index) {
            leaf.unwrap();
        }
    }
}

#[test]
fn an_interrupted_run_leaves_a_valid_store() {
    let all = [&LIVE[..], &tries(), &[STALE, ROOTS]].concat();
    let clean = MemoryStore::new();
    history(&clean, 9, |_| None);
    prune(&clean, &MemoryStore::new(), &options(5)).unwrap();
    for writes in 0.. {
        let store = MemoryStore::new();
        history(&store, 9, |_| None);
        let interrupted = Interrupted {
            store: &store,
            writes: Cell::new(writes),
        };
        if prune(
            &interrupted,
            &MemoryStore::new(),
            &options(5).batch_rows(100),
        )
        .is_ok()
        {
            assert!(writes > 10, "finished in {writes} writes");
            break;
        }
        assert_roots_readable(&store);
        prune(&store, &MemoryStore::new(), &options(5)).unwrap();
        assert_eq!(
            rows(&store, &all),
            rows(&clean, &all),
            "after {writes} writes"
        );
    }
}

#[test]
fn pruning_mdbx_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let store = MdbxStore::open(dir.path()).unwrap();
    history(&store, 5, |_| None);
    with_other_table(&store);
    assert_matches_pruned(&store, 5, 5);
}

#[test]
fn a_nonempty_marks_store_is_refused() {
    let store = MemoryStore::new();
    history(&store, 5, |_| None);
    let marks = MemoryStore::new();
    genesis(&marks);
    let refused = prune(&store, &marks, &options(5));
    assert!(matches!(refused, Err(PruneError::MarksNotEmpty)));
}

#[test]
fn a_window_below_min_retention_is_refused() {
    let store = MemoryStore::new();
    history(&store, 6, |_| None);
    set_min_retention(&store, 5);
    let refused = prune(&store, &MemoryStore::new(), &options(4));
    assert!(matches!(
        refused,
        Err(PruneError::BelowMinRetention { window: 4, min: 5 })
    ));
    prune(&store, &MemoryStore::new(), &options(5)).unwrap();

    let key = CellKey::new(system::PARAMS.id, reserved::MIN_RETENTION);
    let mut tx = store.begin_write().unwrap();
    tx.delete(CELL, &key.encode()).unwrap();
    tx.commit().unwrap();
    let refused = prune(&store, &MemoryStore::new(), &options(5));
    assert!(matches!(refused, Err(PruneError::Corrupt(_))));
}

#[test]
fn the_binary_prunes_and_compacts_a_closed_store_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let store = MdbxStore::open(&path).unwrap();
    history(&store, 7, |_| None);
    let tries_before = rows(&store, &tries()).len();
    let data = path.join("mdbx.dat");
    let mode = 0o640;
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(mode)).unwrap();
    let all = [&LIVE[..], &tries(), &[STALE, ROOTS]].concat();
    let pruned = {
        let pruned = MemoryStore::new();
        history(&pruned, 7, |_| None);
        prune(&pruned, &MemoryStore::new(), &options(5)).unwrap();
        rows(&pruned, &all)
    };
    let scratch = || {
        let entries = std::fs::read_dir(&path).unwrap();
        (entries.map(|entry| entry.unwrap().file_name()))
            .filter(|name| name.to_string_lossy().starts_with("compact-"))
            .count()
    };
    let used = |store: &MdbxStore| store.stats().unwrap().unwrap().db_size;
    let run = |flags: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_golemdb-compact"))
            .arg(&path)
            .args(["--retention-window", "5"])
            .args(flags)
            .output()
            .unwrap()
    };
    let refused = run(&[]);
    assert!(!refused.status.success(), "ran while the store was open");
    assert_eq!(rows(&store, &tries()).len(), tries_before);
    drop(store);

    let output = run(&["--prune-only"]);
    assert!(output.status.success(), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("compacted"));
    assert_eq!(scratch(), 0);
    let store = MdbxStore::open(&path).unwrap();
    assert_eq!(rows(&store, &all), pruned);
    let used_pruned = used(&store);
    drop(store);

    let output = run(&[]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(scratch(), 0);
    let metadata = std::fs::metadata(&data).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, mode);
    let store = MdbxStore::open(&path).unwrap();
    assert!(used(&store) < used_pruned);
    assert_eq!(rows(&store, &all), pruned);
    assert_eq!(rows(&store, &[ROOTS]).len(), 6);
}
