use golemdb_api::*;
use golemdb_cells::tables;
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
    let (plain, plain_head) = run(config());
    let (pruned, pruned_head) = run(config().with_pruning(PruningConfig::new(2)));
    assert_eq!(pruned_head, plain_head);
    assert_eq!(count(&pruned, Table("Roots")), 3);
    let cells = tables::CELL_TRIE.table();
    assert!(count(&pruned, cells) < count(&plain, cells));
}
