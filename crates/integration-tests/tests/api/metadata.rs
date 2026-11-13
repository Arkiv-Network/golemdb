use golemdb_api::*;
use golemdb_cells::system;

use super::facade::each_backend;

const KEY: RecordKey = RecordKey([0x42; 32]);
const OTHER: RecordKey = RecordKey([0x43; 32]);
const BUDGET: Budget = Budget::Unlimited;

fn get(db: &Database, target: ReadTarget, key: RecordKey) -> Record {
    db.get(target, RecordOp::get(key, BUDGET))
        .into_result()
        .unwrap()
}

#[test]
fn metadata_tracks_receipts_kind_changes_removals_rollback_and_query_projections() {
    each_backend(|db| {
        let b = db.begin().unwrap();
        let target = ReadTarget::Branch(b);
        let created = db.create(
            b,
            RecordOp::create(KEY, BUDGET)
                .field("age", CellValue::from_i32(30))
                .attribute("color", CellValue::from_str("red")),
        );
        created.result.unwrap();
        let original = get(&db, target, KEY);
        let meta = RecordMeta {
            cells: 2,
            cell_bytes: 33,
            indexed_cells: 1,
            index_bytes: 10,
        };
        assert_eq!(original.meta(), Some(meta));
        assert_eq!(created.receipt.details.cells_created, meta.cells);
        assert_eq!(created.receipt.details.cell_bytes_written, meta.cell_bytes);
        assert_eq!(created.receipt.details.index_joins, meta.indexed_cells);
        assert_eq!(
            created.receipt.details.index_bytes_written,
            meta.index_bytes
        );
        db.checkpoint(b).unwrap();
        db.patch(
            b,
            RecordOp::patch(KEY, BUDGET).attribute("age", CellValue::from_i32(30)),
        )
        .into_result()
        .unwrap();
        assert_eq!(
            get(&db, target, KEY).meta(),
            Some(RecordMeta {
                indexed_cells: 2,
                index_bytes: 19,
                ..meta
            })
        );
        db.patch(
            b,
            RecordOp::patch(KEY, BUDGET)
                .remove("age")
                .set("color", CellValue::from_str("blue")),
        )
        .into_result()
        .unwrap();
        assert_eq!(
            get(&db, target, KEY).meta(),
            Some(RecordMeta {
                cells: 1,
                cell_bytes: 18,
                indexed_cells: 1,
                index_bytes: 11
            })
        );
        db.patch(
            b,
            RecordOp::patch(KEY, BUDGET).field("color", CellValue::from_str("blue")),
        )
        .into_result()
        .unwrap();
        assert_eq!(
            get(&db, target, KEY).meta(),
            Some(RecordMeta {
                cells: 1,
                cell_bytes: 18,
                indexed_cells: 0,
                index_bytes: 0
            })
        );
        db.patch(b, RecordOp::patch(KEY, BUDGET).remove("color"))
            .into_result()
            .unwrap();
        assert_eq!(get(&db, target, KEY).meta(), Some(RecordMeta::default()));
        let info = db.branch_info(b).unwrap();
        db.patch(b, RecordOp::patch(KEY, BUDGET).remove("missing"))
            .into_result()
            .unwrap();
        assert_eq!(db.branch_info(b).unwrap(), info);
        db.delete(b, RecordOp::delete(KEY, BUDGET))
            .into_result()
            .unwrap();
        assert!(matches!(
            db.get(target, RecordOp::get(KEY, BUDGET)).result,
            Err(ApiError::NotFound)
        ));
        db.rollback(b).unwrap();
        assert_eq!(get(&db, target, KEY), original);
        db.commit(b).unwrap();
        assert_eq!(get(&db, ReadTarget::Head, KEY), original);

        let query = || {
            QueryOp::from_filter(
                Filter::Eq {
                    field: CellNameRef::raw(b"color").into(),
                    value: EqValue::of(marker::Str, "red"),
                },
                BUDGET,
            )
        };
        assert_eq!(
            db.query(None, query()).into_result().unwrap().records,
            [original]
        );
        for projection in [vec!["#meta"], vec!["color"], vec![]] {
            let record = db
                .get(
                    ReadTarget::Head,
                    RecordOp::get(KEY, BUDGET).only(projection.clone()),
                )
                .into_result()
                .unwrap();
            assert_eq!(record.meta(), projection.contains(&"#meta").then_some(meta));
            assert_eq!(
                db.query(None, query().only(projection))
                    .into_result()
                    .unwrap()
                    .records,
                [record]
            );
        }
        for reserved in system::ALL {
            let record = get(&db, ReadTarget::Head, RecordKey(reserved.key));
            assert_eq!(record.meta(), None);
            assert!(!record.cells.contains_key(b"#meta".as_slice()));
        }
    });
}

fn cap_contract(db: Database) {
    let b = db.begin().unwrap();
    let target = ReadTarget::Branch(b);
    let allocator = || get(&db, target, RecordKey(system::ALLOC.key));
    let initial_allocator = allocator();
    let initial_info = db.branch_info(b).unwrap();
    for create in [
        RecordOp::create(KEY, BUDGET)
            .field("a", CellValue::from_bool(true))
            .field("b", CellValue::from_bool(true))
            .field("c", CellValue::from_bool(true)),
        RecordOp::create(KEY, BUDGET)
            .attribute("a", CellValue::from_bool(true))
            .attribute("b", CellValue::from_bool(true)),
    ] {
        let call = db.create(b, create);
        assert!(matches!(call.result, Err(ApiError::InvalidArgument { .. })));
        assert_eq!(call.receipt.details, Details::default());
        assert_eq!(db.branch_info(b).unwrap(), initial_info);
        assert_eq!(allocator(), initial_allocator);
    }
    db.create(
        b,
        RecordOp::create(KEY, BUDGET)
            .attribute("z", CellValue::from_bool(true))
            .field("y", CellValue::from_bool(true)),
    )
    .into_result()
    .unwrap();
    let original = get(&db, target, KEY);
    let info = db.branch_info(b).unwrap();
    let allocated = allocator();
    for patch in [
        RecordOp::patch(KEY, BUDGET).field("extra", CellValue::from_bool(true)),
        RecordOp::patch(KEY, BUDGET).attribute("y", CellValue::from_bool(true)),
    ] {
        let call = db.patch(b, patch);
        assert!(matches!(call.result, Err(ApiError::InvalidArgument { .. })));
        assert_eq!(call.receipt.details, Details::default());
        assert_eq!(get(&db, target, KEY), original);
        assert_eq!(db.branch_info(b).unwrap(), info);
        assert_eq!(allocator(), allocated);
    }
    // In name order the addition precedes the removal. Both caps must be
    // checked against the final record, not a transient intermediate count.
    db.patch(
        b,
        RecordOp::patch(KEY, BUDGET)
            .attribute("a", CellValue::from_bool(true))
            .remove("z"),
    )
    .into_result()
    .unwrap();
    assert_eq!(get(&db, target, KEY).meta(), original.meta());
    db.create(b, RecordOp::create(OTHER, BUDGET))
        .into_result()
        .unwrap();
    assert_eq!(get(&db, target, OTHER).meta(), Some(RecordMeta::default()));
    db.commit(b).unwrap();
}

#[test]
fn genesis_caps_bound_final_records_without_consuming_ids_on_failure() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let genesis = Genesis::new(
            hash,
            CellLimits {
                max_cell_name_len: 32,
                max_str_len: 64,
                max_bytes_len: 128,
            },
            RecordLimits {
                max_cells: 2,
                max_indexed_cells: 1,
            },
            None,
            golemdb_api::PricingConfig::zero(),
        );
        cap_contract(Database::open_memory(&genesis).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let config = OpenConfig::new(genesis);
        cap_contract(Database::open(dir.path(), &config).unwrap());
        let reopened = Database::open(dir.path(), &config).unwrap();
        assert_eq!(
            get(&reopened, ReadTarget::Head, KEY).meta().unwrap().cells,
            2
        );
    }
}

#[test]
fn zero_caps_allow_empty_records_and_do_not_limit_system_records() {
    let genesis = Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 1,
            max_str_len: 0,
            max_bytes_len: 0,
        },
        RecordLimits {
            max_cells: 0,
            max_indexed_cells: 0,
        },
        None,
        golemdb_api::PricingConfig::zero(),
    );
    let db = Database::open_memory(&genesis).unwrap();
    let b = db.begin().unwrap();
    db.create(b, RecordOp::create(KEY, BUDGET))
        .into_result()
        .unwrap();
    assert_eq!(
        get(&db, ReadTarget::Branch(b), KEY).meta(),
        Some(RecordMeta::default())
    );
    assert!(matches!(
        db.patch(
            b,
            RecordOp::patch(KEY, BUDGET).field("x", CellValue::from_bool(true))
        )
        .result,
        Err(ApiError::InvalidArgument { .. })
    ));
    db.delete(b, RecordOp::delete(KEY, BUDGET))
        .into_result()
        .unwrap();
    db.commit(b).unwrap();
}
