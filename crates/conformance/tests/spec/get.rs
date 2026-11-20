//! Record operations: `get`.

use golemdb_conformance::*;

fn setup() -> TestDb {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key(1), B)
            .attribute("a", CellValue::from_u32(1))
            .field("b", CellValue::from_str("x")),
    )
    .result
    .unwrap();
    db.commit(branch).unwrap();
    db
}

/// "A missing record is `NotFound`."
#[test]
fn missing_record_is_not_found_on_every_target() {
    let db = setup();
    let branch = db.begin().unwrap();
    let head = db.head().unwrap();
    for target in [
        ReadTarget::Head,
        ReadTarget::Branch(branch),
        ReadTarget::Commit(head),
    ] {
        // Full read and projection alike.
        assert_err!(
            db.get(target, RecordOp::get(key(9), B)).result,
            ApiError::NotFound
        );
        assert_err!(
            db.get(target, RecordOp::get(key(9), B).only(["a"])).result,
            ApiError::NotFound
        );
    }
}

/// "With `.only(names)`, missing cells are omitted, and an empty list returns no
/// cells but still checks that the record exists."
#[test]
fn projection_omits_missing_cells_and_empty_still_checks_existence() {
    let db = setup();
    let record = db
        .get(
            ReadTarget::Head,
            RecordOp::get(key(1), B).only(["b", "nope", "#key"]),
        )
        .result
        .unwrap();
    // Only the named cells that exist, in name order.
    let names: Vec<_> = record.cells.keys().map(|n| n.as_bytes().to_vec()).collect();
    assert_eq!(names, [b"#key".to_vec(), b"b".to_vec()]);
    assert_eq!(record.key, key(1));

    // An empty projection: no cells for an existing record, `NotFound` for a
    // missing one.
    let no_cells = |k| RecordOp::get(k, B).only(std::iter::empty::<&str>());
    assert!(
        db.get(ReadTarget::Head, no_cells(key(1)))
            .result
            .unwrap()
            .cells
            .is_empty()
    );
    assert_err!(
        db.get(ReadTarget::Head, no_cells(key(2))).result,
        ApiError::NotFound
    );
}
