//! Record operations: `create`.
//!
//! Where the code breaks the spec, the test is `#[ignore]`d with the reason.
//!
//! Agreed changes the code already implements, tested as agreed:
//! - the handle before admission: the spec says the implementations check reserved
//!   keys before the handle; the code checks the handle first.
//!
//! Where the spec is stale or silent, so nothing here tests it:
//! - Key modes: the spec has `RecordOp::create(budget).key(k)`, generated keys,
//!   `KeyModeMismatch` and the `.key` given twice error; the code takes the key in
//!   `RecordOp::create(key, budget)` and has caller-assigned keys only.
//! - Per-record caps: the spec lists `#maxRecordCells` and `#maxRecordIndexedCells`
//!   as not implemented; the code enforces them with `InvalidArgument`.
//! - `RecordOp::validate()`, `cells()` and `value()` are in the spec, not the code;
//!   the code has `entries()` instead of `cells()`, in caller order with duplicates
//!   kept.
//! - Reserved keys: the spec reserves records 0–63 but rejects only keys equal to a
//!   catalogue key; whether another zero-padded `#…` key, or the all-zero key, is
//!   reserved is open. The code accepts both as user keys.

use golemdb_conformance::*;

/// A create that every later stage would also reject.
fn bad_everywhere() -> RecordOp<op::Create> {
    RecordOp::create(reserved_key("#alloc"), B).field("x y", CellValue::from_u32(1))
}

/// "The first problem is kept and reported by the call as `InvalidArgument`, before
/// the branch is touched."
/// "| a name violating the grammar | `attribute("x y"): invalid cell name`, cause as
/// source |"
/// "| a value that cannot convert (NaN) or cannot take the kind (`bytes` as
/// attribute) | `field("f"): invalid cell value`, cause as source |"
/// "| a name written or removed twice in one operation | `remove("x"): the operation
/// already writes or removes this cell` |"
#[test]
fn builder_errors_name_the_step_and_the_problem() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    let u32 = CellValue::from_u32(1);
    // The operation, its message and whether it carries a cause.
    for (op, expected, cause) in [
        (
            RecordOp::create(key(1), B).attribute("x y", u32.clone()),
            r#"attribute("x y"): invalid cell name"#,
            true,
        ),
        (
            RecordOp::create(key(1), B).attribute("f", CellValue::from_bytes(b"a")),
            r#"attribute("f"): invalid cell value"#,
            true,
        ),
        (
            RecordOp::create(key(1), B)
                .field("x", u32.clone())
                .field("x", u32.clone()),
            r#"field("x"): the operation already writes or removes this cell"#,
            false,
        ),
        // Of two problems, the first given is reported, not the first by name.
        (
            RecordOp::create(key(1), B)
                .field("y z", u32.clone())
                .field("x y", u32.clone()),
            r#"field("y z"): invalid cell name"#,
            true,
        ),
        // A duplicate across kinds is still a duplicate.
        (
            RecordOp::create(key(1), B)
                .attribute("x", u32.clone())
                .field("x", u32.clone()),
            r#"field("x"): the operation already writes or removes this cell"#,
            false,
        ),
    ] {
        assert_err!(
            db.create(branch, op).result,
            ApiError::InvalidArgument { message, source }
                if message == expected && source.is_some() == cause
        );
    }
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
            db.version(branch),
            db.next_record_id(branch),
            db.read(ReadTarget::Branch(branch), key(1)),
        )
    };
    // Version, allocator and the existing record.
    let before = state();

    // Admission: a builder error, a reserved key, an over-long value.
    assert_err!(
        db.create(
            branch,
            RecordOp::create(key(2), B).field("x y", CellValue::from_u32(1))
        )
        .result,
        ApiError::InvalidArgument { .. }
    );
    assert_err!(
        db.create(branch, RecordOp::create(reserved_key("#alloc"), B))
            .result,
        ApiError::Reserved
    );
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
    // Record state: the key is already bound.
    assert_err!(
        db.create(
            branch,
            RecordOp::create(key(1), B).field("a", CellValue::from_u32(1))
        )
        .result,
        ApiError::AlreadyExists
    );

    // No failure changed those, nor bound the new key.
    assert_eq!(state(), before);
    assert_err!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(key(2), B))
            .result,
        ApiError::NotFound
    );
}

/// "1. Handle: unknown or consumed handle, or stale origin → `HandleInvalid`;
/// sealed → `Sealed`."
#[test]
fn handle_errors_come_first() {
    let db = TestDb::new();
    let bind = db.begin().unwrap();
    db.create(bind, RecordOp::create(key(1), B)).result.unwrap();
    db.commit(bind).unwrap();
    // A create that admission would reject (a reserved key and a bad name), and one
    // that record state would (`key(1)` is bound).
    let ops = || [bad_everywhere(), RecordOp::create(key(1), B)];
    // On a stale branch: one per op, as the first call that detects one removes it.
    for op in ops() {
        let stale = db.begin().unwrap();
        db.commit(db.begin().unwrap()).unwrap();
        assert_err!(db.create(stale, op).result, ApiError::HandleInvalid);
    }
    let committed = db.begin().unwrap();
    db.commit(committed).unwrap();
    let discarded = db.begin().unwrap();
    db.discard(discarded).unwrap();
    let sealed = db.begin().unwrap();
    db.seal(sealed).unwrap();
    // On a committed, discarded or unknown branch.
    for branch in [committed, discarded, BranchId::new(u64::MAX)] {
        for op in ops() {
            assert_err!(db.create(branch, op).result, ApiError::HandleInvalid);
        }
    }
    // On a sealed branch.
    for op in ops() {
        assert_err!(db.create(sealed, op).result, ApiError::Sealed);
    }
}

/// "2. Admission: 1. builder error → `InvalidArgument`;"
/// "4. each name against `#maxCellNameLen` and each `str` / `bytes` value against
/// its limit, in name order → `InvalidArgument`."
/// "3. Record state: … 2. key already bound → `AlreadyExists` in caller-assigned
/// mode"
#[test]
fn admission_errors_come_before_already_exists() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    db.create(branch, RecordOp::create(key(1), B))
        .result
        .unwrap();
    db.commit(branch).unwrap();
    let branch = db.begin().unwrap();
    db.create(branch, RecordOp::create(key(2), B))
        .result
        .unwrap();
    // `key(1)` is bound in the store, `key(2)` only in the branch.
    for k in [key(1), key(2)] {
        // An over-long value or name fails admission first.
        assert_err!(
            db.create(
                branch,
                RecordOp::create(k, B).field("s", CellValue::from_str(&"a".repeat(65)))
            )
            .result,
            ApiError::InvalidArgument { .. }
        );
        assert_err!(
            db.create(
                branch,
                RecordOp::create(k, B).field(&"n".repeat(33), CellValue::from_u32(1))
            )
            .result,
            ApiError::InvalidArgument { .. }
        );
        // So does a builder error: a bad name, or `bytes` as an attribute.
        assert_err!(
            db.create(
                branch,
                RecordOp::create(k, B).field("x y", CellValue::from_u32(1))
            )
            .result,
            ApiError::InvalidArgument { .. }
        );
        assert_err!(
            db.create(
                branch,
                RecordOp::create(k, B).attribute("f", CellValue::from_bytes(b"a"))
            )
            .result,
            ApiError::InvalidArgument { .. }
        );
        // With valid cells, the bound key is reported.
        assert_err!(
            db.create(branch, RecordOp::create(k, B)).result,
            ApiError::AlreadyExists
        );
    }
}

/// "1. builder error → `InvalidArgument`;"
/// "2. caller-assigned key equal to a reserved key → `Reserved`;"
#[test]
#[ignore = "admission checks the reserved key before builder errors"]
fn builder_errors_come_before_reserved() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    assert_err!(
        db.create(branch, bad_everywhere()).result,
        ApiError::InvalidArgument { .. }
    );
}

/// "1. builder error → `InvalidArgument`;"
/// "4. each name against `#maxCellNameLen` and each `str` / `bytes` value against
/// its limit, in name order → `InvalidArgument`."
#[test]
#[ignore = "admission checks an entry's limits before its grammar"]
fn builder_errors_come_before_limits() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    let long = || CellValue::from_str(&"a".repeat(65));
    // The bad name on the over-long value itself, or after another over-long value.
    for op in [
        RecordOp::create(key(1), B).field("a b", long()),
        RecordOp::create(key(1), B)
            .field("b", long())
            .field("a b", CellValue::from_u32(1)),
    ] {
        assert_err!(
            db.create(branch, op).result,
            ApiError::InvalidArgument { message, .. } if message == r#"field("a b"): invalid cell name"#
        );
    }
}

/// "1. builder error → `InvalidArgument`;"
/// "4. each name against `#maxCellNameLen` and each `str` / `bytes` value against
/// its limit, in name order → `InvalidArgument`."
/// "| a name written or removed twice in one operation | `remove("x"): the operation
/// already writes or removes this cell` |"
#[test]
#[ignore = "admission checks an entry's limits before duplicates"]
fn duplicates_come_before_limits() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    // The duplicate step's value is over-long.
    assert_err!(
        db.create(
            branch,
            RecordOp::create(key(1), B)
                .field("x", CellValue::from_u32(1))
                .field("x", CellValue::from_str(&"a".repeat(65)))
        )
        .result,
        ApiError::InvalidArgument { message, .. }
            if message == r#"field("x"): the operation already writes or removes this cell"#
    );
}

/// "2. caller-assigned key equal to a reserved key → `Reserved`;"
#[test]
fn reserved_keys_are_reserved() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    for name in RESERVED_NAMES {
        assert_err!(
            db.create(branch, RecordOp::create(reserved_key(name), B))
                .result,
            ApiError::Reserved
        );
    }
    // A near miss, `#params` padded with 0x01 instead of zeros, is a user key.
    let mut near = reserved_key("#params").0;
    near[7..].fill(1);
    db.create(branch, RecordOp::create(RecordKey(near), B))
        .result
        .unwrap();
}

/// "2. caller-assigned key equal to a reserved key → `Reserved`;"
/// "4. each name against `#maxCellNameLen` and each `str` / `bytes` value against
/// its limit, in name order → `InvalidArgument`."
#[test]
fn reserved_comes_before_limits() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    assert_err!(
        db.create(
            branch,
            RecordOp::create(reserved_key("#alloc"), B)
                .field("s", CellValue::from_str(&"a".repeat(65)))
        )
        .result,
        ApiError::Reserved
    );
}

/// "4. each name against `#maxCellNameLen` and each `str` / `bytes` value against
/// its limit, in name order → `InvalidArgument`."
#[test]
fn limits_are_inclusive() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    // A name, a `str` and a `bytes` value at their limits are accepted, the first
    // two also as one attribute.
    db.create(
        branch,
        RecordOp::create(key(1), B)
            .field(&"n".repeat(32), CellValue::from_u32(1))
            .field("s", CellValue::from_str(&"a".repeat(64)))
            .field("b", CellValue::from_bytes(&[0; 128]))
            .attribute(&"m".repeat(32), CellValue::from_str(&"a".repeat(64))),
    )
    .result
    .unwrap();
    // One byte over each is rejected.
    for op in [
        RecordOp::create(key(2), B).field(&"n".repeat(33), CellValue::from_u32(1)),
        RecordOp::create(key(2), B).field("s", CellValue::from_str(&"a".repeat(65))),
        RecordOp::create(key(2), B).attribute("s", CellValue::from_str(&"a".repeat(65))),
        RecordOp::create(key(2), B).field("b", CellValue::from_bytes(&[0; 129])),
    ] {
        assert_err!(
            db.create(branch, op).result,
            ApiError::InvalidArgument { .. }
        );
    }
}

/// "4. each name against `#maxCellNameLen` and each `str` / `bytes` value against
/// its limit, in name order → `InvalidArgument`."
#[test]
#[ignore = "admission checks limits in the caller's order"]
fn limits_are_checked_in_name_order() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    let long = CellValue::from_str(&"a".repeat(65));
    // `z` is given first, but `a` comes first by name.
    assert_err!(
        db.create(
            branch,
            RecordOp::create(key(1), B)
                .field("z", long.clone())
                .field("a", long)
        )
        .result,
        ApiError::InvalidArgument { message, .. } if message.starts_with(r#"field("a")"#)
    );
}

/// "On success it writes the user cells, `#key`, `#meta`, the binding and the
/// advanced allocator as one atomic branch operation, and returns the key."
#[test]
fn create_writes_the_record_and_advances_the_allocator() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    // Genesis starts the allocator at 64.
    assert_eq!(db.next_record_id(branch), Some(64));
    let k = db
        .create(
            branch,
            RecordOp::create(key(1), B).field("a", CellValue::from_u32(1)),
        )
        .result
        .unwrap();
    // Returns the caller's key, bound to the first user record ID.
    assert_eq!(k, key(1));
    assert_eq!(db.record_id(branch, k), Some(64));
    // The binding is a field.
    let bindings = db.read(ReadTarget::Branch(branch), reserved_key("#recordKeys"));
    assert_eq!(
        bindings.cells.get(k.0.as_slice()).map(CellValue::kind),
        Some(CellKind::Field)
    );
    // The binding resolves the key to the user cells, `#key` and `#meta`.
    let record = db.read(ReadTarget::Branch(branch), k);
    let user_cells = user_cells(&record);
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
    assert_eq!(db.next_record_id(branch), Some(65));
}

/// "On success it writes the user cells, `#key`, `#meta`, the binding and the
/// advanced allocator as one atomic branch operation, and returns the key."
#[test]
fn many_creates_in_one_commit_read_back_independently() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    let cells_of = |n: u8| {
        (
            CellValue::from_u32(n.into()),
            CellValue::from_str(&format!("record {n}")),
        )
    };
    for n in 1..=100 {
        let (number, label) = cells_of(n);
        db.create(
            branch,
            RecordOp::create(key(n), B)
                .attribute("n", number)
                .field("label", label),
        )
        .result
        .unwrap();
    }
    db.commit(branch).unwrap();
    // Each record has its own cells and the next ID, in creation order.
    let branch = db.begin().unwrap();
    for n in 1..=100 {
        let (number, label) = cells_of(n);
        let record = db.read(ReadTarget::Head, key(n));
        assert_eq!(record.cells.get("label".as_bytes()), Some(&label));
        assert_eq!(record.cells.get("n".as_bytes()), Some(&attribute(number)));
        assert_eq!(db.record_id(branch, key(n)), Some(63 + u64::from(n)));
    }
}
