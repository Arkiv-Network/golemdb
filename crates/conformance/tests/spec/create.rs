//! Record operations: `create`.

use golemdb_conformance::*;

/// `#nextRecordID` as `branch` sees it.
fn next_record_id(db: &TestDb, branch: BranchId) -> Option<u64> {
    // A reserved record's key is its name, ASCII, zero-padded to 32 bytes.
    let mut alloc = [0; 32];
    alloc[..6].copy_from_slice(b"#alloc");
    db.read(ReadTarget::Branch(branch), RecordKey(alloc))
        .cells
        .get("#nextRecordID".as_bytes())?
        .as_u64()
}

/// "On success it writes the user cells, `#key`, `#meta`, the binding and the
/// advanced allocator as one atomic branch operation, and returns the key."
#[test]
fn create_writes_the_record_and_advances_the_allocator() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    // Genesis starts the allocator at 64.
    assert_eq!(next_record_id(&db, branch), Some(64));
    let k = db
        .create(
            branch,
            RecordOp::create(key(1), B).field("a", CellValue::from_u32(1)),
        )
        .result
        .unwrap();
    // Returns the caller's key.
    assert_eq!(k, key(1));
    // The binding resolves the key to the user cells, `#key` and `#meta`.
    let record = db.read(ReadTarget::Branch(branch), k);
    let user_cells: Vec<_> = record
        .cells
        .iter()
        .filter(|(name, _)| !name.as_bytes().starts_with(b"#"))
        .map(|(name, value)| (name.as_bytes(), value))
        .collect();
    assert_eq!(user_cells, [(b"a".as_slice(), &CellValue::from_u32(1))]);
    assert_eq!(
        record
            .cells
            .get("#key".as_bytes())
            .and_then(|v| v.as_bytes32()),
        Some(k.0)
    );
    assert!(record.cells.contains_key("#meta".as_bytes()));
    // The allocator advanced by one.
    assert_eq!(next_record_id(&db, branch), Some(65));
}

/// "Order of checks ([three stages](#order-of-checks)); the first failure is
/// returned and nothing is written"
#[test]
fn a_failed_create_writes_nothing() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    db.create(branch, RecordOp::create(key(1), B))
        .result
        .unwrap();
    let state = || {
        (
            db.branch_info(branch).unwrap().version,
            next_record_id(&db, branch),
            db.read(ReadTarget::Branch(branch), key(1)),
        )
    };
    // Version, allocator and the existing record.
    let before = state();

    // Admission: an over-long value.
    assert_err!(
        db.create(
            branch,
            RecordOp::create(key(2), B)
                .field("ok", CellValue::from_u32(1))
                .field("s", CellValue::from_str(&"a".repeat(65)))
        )
        .result,
        ApiError::InvalidArgument { .. }
    );
    // Record state, after the ID is allocated: the key is already bound.
    assert_err!(
        db.create(
            branch,
            RecordOp::create(key(1), B).field("a", CellValue::from_u32(1))
        )
        .result,
        ApiError::AlreadyExists
    );

    // Neither failure changed those, nor bound the new key.
    assert_eq!(state(), before);
    assert_err!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(key(2), B))
            .result,
        ApiError::NotFound
    );
}
