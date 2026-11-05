use golemdb_api::*;
use golemdb_cells::tables;
use golemdb_index::tables::{BITMAP_CONTAINER, BITMAP_TRIE, INDEX, INDEX_TRIE};
use golemdb_pruning::STALE;
use golemdb_storage::{MemoryStore, Store, Table, scan_prefix};

const KEY: RecordKey = RecordKey([0x42; 32]);
const BUDGET: Budget = Budget::Limited(u64::MAX);
/// Past the wait after which rows go without a full batch.
const COMMITS: i32 = golemdb_pruning::MAX_WAIT_COMMITS as i32 + 10;

fn config() -> OpenConfig {
    OpenConfig::new(Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
        RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
        None,
        PricingConfig::zero(),
        0,
    ))
}

fn count(db: &MemoryStore, table: Table) -> usize {
    let tx = db.begin_read().unwrap();
    scan_prefix(&tx, table, vec![]).unwrap().count()
}

/// Commits patching one record, past the wait before rows go without a full
/// batch: the run with pruning keeps only the window's `Roots` and fewer trie
/// rows, and both reach the same head.
#[test]
fn open_config_pruning_deletes_superseded_rows() {
    let run = |config: OpenConfig| {
        let db = MemoryStore::new();
        let api = Database::from_store(db.clone(), &config).unwrap();
        let branch = api.begin().unwrap();
        let create = RecordOp::create(KEY, BUDGET).attribute("price", CellValue::from_i32(0));
        api.create(branch, create).into_result().unwrap();
        api.commit(branch).unwrap();
        for price in 1..COMMITS {
            let branch = api.begin().unwrap();
            let patch = RecordOp::patch(KEY, BUDGET).attribute("price", CellValue::from_i32(price));
            api.patch(branch, patch).into_result().unwrap();
            api.commit(branch).unwrap();
        }
        let record = api.get(ReadTarget::Head, RecordOp::get(KEY, BUDGET));
        let price = record.into_result().unwrap().cells[b"price".as_slice()].as_i32();
        assert_eq!(price, Some(COMMITS - 1));
        (db, api.head().unwrap())
    };
    let (plain, plain_head) = run(config().without_pruning());
    let (pruned, pruned_head) = run(config().with_pruning(PruningConfig::new(2)));
    assert_eq!(pruned_head, plain_head);
    assert_eq!(count(&pruned, Table("Roots")), 3);
    let cells = tables::CELL_TRIE.table();
    assert!(count(&pruned, cells) < count(&plain, cells));
}

/// Creates the record at price 0 and patches it after, one commit each.
fn commit(api: &Database, price: i32) {
    let branch = api.begin().unwrap();
    let value = CellValue::from_i32(price);
    if price == 0 {
        let create = RecordOp::create(KEY, BUDGET).attribute("price", value);
        api.create(branch, create).into_result().unwrap();
    } else {
        let patch = RecordOp::patch(KEY, BUDGET).attribute("price", value);
        api.patch(branch, patch).into_result().unwrap();
    }
    api.commit(branch).unwrap();
}

fn rows(db: &MemoryStore, table: Table) -> Vec<(Vec<u8>, Vec<u8>)> {
    let tx = db.begin_read().unwrap();
    (scan_prefix(&tx, table, vec![]).unwrap())
        .map(|row| row.unwrap())
        .collect()
}

fn with_min_retention(commits: u64) -> OpenConfig {
    let mut genesis = config().genesis;
    genesis.min_retention = commits;
    OpenConfig::new(genesis)
}

/// A window below `#minRetention` is refused before genesis is written.
#[test]
fn window_below_min_retention_is_refused() {
    let db = MemoryStore::new();
    let open = with_min_retention(5).with_pruning(PruningConfig::new(4));
    let opened = Database::from_store(db.clone(), &open);
    assert!(matches!(opened, Err(OpenError::InvalidConfig(_))));
    assert_eq!(count(&db, Table("Superblock")), 0);
}

/// Without pruning settings a node keeps `#minRetention` commits before the
/// head.
#[test]
fn default_pruning_keeps_min_retention() {
    let db = MemoryStore::new();
    let api = Database::from_store(db.clone(), &with_min_retention(3)).unwrap();
    for price in 0..COMMITS {
        commit(&api, price);
    }
    assert_eq!(count(&db, Table("Roots")), 4);
}

/// `#minRetention` is part of the genesis identity, and a mismatch is reported
/// as one even when the requested window is below the requested minimum.
#[test]
fn min_retention_is_in_genesis_identity() {
    let id = |commits| {
        let api = Database::from_store(MemoryStore::new(), &with_min_retention(commits));
        api.unwrap().info().genesis_id
    };
    assert_ne!(id(0), id(3));
    let db = MemoryStore::new();
    Database::from_store(db.clone(), &with_min_retention(3)).unwrap();
    let open = with_min_retention(4).with_pruning(PruningConfig::new(3));
    let reopened = Database::from_store(db, &open);
    assert!(matches!(reopened, Err(OpenError::GenesisMismatch)));
}

/// A larger window after a reopen keeps only the commits still retained and
/// refills as commits continue.
#[test]
fn grown_window_does_not_restore_pruned_commits() {
    let db = MemoryStore::new();
    let small = config().with_pruning(PruningConfig::new(2));
    let api = Database::from_store(db.clone(), &small).unwrap();
    for price in 0..COMMITS {
        commit(&api, price);
    }
    drop(api);
    let large = config().with_pruning(PruningConfig::new(10));
    let api = Database::from_store(db.clone(), &large).unwrap();
    assert_eq!(count(&db, Table("Roots")), 3);
    commit(&api, COMMITS);
    assert_eq!(count(&db, Table("Roots")), 4);
}

/// A reopen with the same settings prunes as an uninterrupted run does.
#[test]
fn reopen_with_same_settings_prunes_identically() {
    let open = config().with_pruning(PruningConfig::new(2));
    let run = |reopen_at: i32| {
        let db = MemoryStore::new();
        let mut api = Database::from_store(db.clone(), &open).unwrap();
        for price in 0..COMMITS {
            if price == reopen_at {
                drop(api);
                api = Database::from_store(db.clone(), &open).unwrap();
            }
            commit(&api, price);
        }
        db
    };
    let (once, reopened) = (run(-1), run(COMMITS / 2));
    let tables = [
        Table("Roots"),
        STALE,
        tables::CELL_TRIE.table(),
        INDEX,
        INDEX_TRIE.table(),
        BITMAP_TRIE.table(),
        BITMAP_CONTAINER.table(),
    ];
    for table in tables {
        assert_eq!(rows(&once, table), rows(&reopened, table));
    }
}
