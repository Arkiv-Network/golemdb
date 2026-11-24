//! Errors.
//!
//! Where the code breaks the spec, the test is `#[ignore]`d with the reason. That
//! includes `Conflict`, "not raised today": a `commit` that is the first call to
//! find its branch stale raises it, which the spec lists as an agreed change, not
//! yet implemented (D15).
//!
//! Where the spec is stale or silent, so nothing here tests it:
//! - `NotImplemented`: the spec raises it for immutable-data calls, which the code
//!   implements.
//! - `KeyModeMismatch`: the code has caller-assigned keys only, and no such variant.
//! - `InvalidQuery` and `LimitExceeded`: the code raises them for queries; the spec
//!   lists them only under "Further errors", as not yet implemented.
//! - `Pruned`: the spec lists it under "Further errors"; the code has no such
//!   variant.
//!
//! Tested elsewhere:
//! - `NotFound`, `CellNotFound` and `AlreadyExists`: the `records` modules.
//! - `OutOfBudget`: `receipts`.
//! - `Reserved` and builder `InvalidArgument`s: the `records` modules; names and
//!   values over a genesis limit: `data_model`.
//! - `HandleInvalid` on record calls: the `records` modules.
//! - `Sealed` on record writes: the `records` modules.
//! - `CommitUnavailable { requested, head }`: `records::get`.
//!
//! Not covered here:
//! - `ApiError` being non-exhaustive: a property of the type, not behaviour.
//! - `HandleInvalid` for a commit that loses the race inside the store writer,
//!   after its handle was checked (D15): one thread cannot make it happen.
//! - `StoreFull` "must never become part of a result other nodes see": a matter of
//!   how hosts use it, not visible from one database.
//! - `Internal`: it needs corrupt state or a failing store.

use golemdb_conformance::*;

/// "invalid input and internal failures keep their causes as sources."
/// "`InvalidArgument { message, source }` | a builder problem, or a name or value
/// over a genesis limit"
#[test]
fn input_over_a_genesis_limit_keeps_its_cause() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key(1), B).field("a", CellValue::from_u32(1)),
    )
    .result
    .unwrap();
    let long_name = "a".repeat(LIMITS.max_cell_name_len as usize + 1);
    let over = [
        (long_name.as_str(), CellValue::from_u32(1)),
        (
            "a",
            CellValue::from_str(&"a".repeat(LIMITS.max_str_len as usize + 1)),
        ),
        (
            "a",
            CellValue::from_bytes(&vec![7; LIMITS.max_bytes_len as usize + 1]),
        ),
    ];
    for (name, value) in over {
        let created = db
            .create(
                branch,
                RecordOp::create(key(2), B).field(name, value.clone()),
            )
            .result;
        let patched = db
            .patch(branch, RecordOp::patch(key(1), B).field(name, value))
            .result;
        for error in [created.unwrap_err(), patched.unwrap_err()] {
            assert!(
                matches!(error, ApiError::InvalidArgument { .. }),
                "{error:?}"
            );
            assert!(std::error::Error::source(&error).is_some(), "{error:?}");
        }
    }
}

/// A branch left stale by a commit of another branch begun over the same head.
fn stale(db: &Database) -> BranchId {
    let (winner, stale) = (db.begin().unwrap(), db.begin().unwrap());
    db.commit(winner).unwrap();
    stale
}

/// A branch sealed, then left stale as [`stale`] does.
fn sealed_stale(db: &Database) -> BranchId {
    let (winner, sealed) = (db.begin().unwrap(), db.begin().unwrap());
    db.seal(sealed).unwrap();
    db.commit(winner).unwrap();
    sealed
}

/// "`HandleInvalid` | an unknown, consumed or stale branch handle, on any call
/// including `commit`"
#[test]
fn every_branch_call_on_a_dead_handle_is_handle_invalid() {
    let db = TestDb::new();
    let unknown = BranchId::new(u64::MAX);
    let committed = db.begin().unwrap();
    db.commit(committed).unwrap();
    let discarded = db.begin().unwrap();
    db.discard(discarded).unwrap();
    let sealed_discarded = db.begin().unwrap();
    db.seal(sealed_discarded).unwrap();
    db.discard(sealed_discarded).unwrap();
    for branch in [unknown, committed, discarded, sealed_discarded] {
        assert_err!(db.branch_info(branch), ApiError::HandleInvalid);
        assert_err!(db.checkpoint(branch), ApiError::HandleInvalid);
        assert_err!(db.rollback(branch), ApiError::HandleInvalid);
        assert_err!(db.seal(branch), ApiError::HandleInvalid);
        assert_err!(db.commit(branch), ApiError::HandleInvalid);
        assert_err!(db.discard(branch), ApiError::HandleInvalid);
    }
    // Each call gets its own, so it is the one to find the branch stale.
    assert_err!(db.branch_info(stale(&db)), ApiError::HandleInvalid);
    assert_err!(db.checkpoint(stale(&db)), ApiError::HandleInvalid);
    assert_err!(db.rollback(stale(&db)), ApiError::HandleInvalid);
    assert_err!(db.seal(stale(&db)), ApiError::HandleInvalid);
    assert_err!(db.discard(stale(&db)), ApiError::HandleInvalid);
    // A commit after another call found the branch stale.
    let branch = stale(&db);
    assert_err!(db.branch_info(branch), ApiError::HandleInvalid);
    assert_err!(db.commit(branch), ApiError::HandleInvalid);
}

/// "`HandleInvalid` | … stale branch handle, on any call including `commit`"
/// "`Conflict` | … not raised today"
#[test]
#[ignore = "commit on a stale branch returns Conflict (D15, agreed, not yet implemented)"]
fn commit_on_a_stale_branch_is_handle_invalid() {
    let db = TestDb::new();
    assert_err!(db.commit(stale(&db)), ApiError::HandleInvalid);
    assert_err!(db.commit(sealed_stale(&db)), ApiError::HandleInvalid);
}

/// "`Sealed` | a record write, checkpoint or rollback on a sealed branch; the
/// handle stays valid"
#[test]
fn a_sealed_branch_rejects_checkpoint_and_rollback_and_stays_valid() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key(1), B).field("a", CellValue::from_u32(1)),
    )
    .result
    .unwrap();
    let sealed = db.seal(branch).unwrap();
    let version = db.branch_info(branch).unwrap().version;
    assert_err!(db.checkpoint(branch), ApiError::Sealed);
    assert_err!(db.rollback(branch), ApiError::Sealed);
    // The rejected calls changed nothing.
    let info = db.branch_info(branch).unwrap();
    assert!(info.sealed);
    assert_eq!(info.version, version);
    assert_eq!(db.seal(branch).unwrap(), sealed);
    db.get(ReadTarget::Branch(branch), RecordOp::get(key(1), B))
        .result
        .unwrap();
    db.commit(branch).unwrap();
}

/// "`HandleInvalid` | an unknown, consumed or stale branch handle"
/// "`Sealed` | a record write, checkpoint or rollback on a sealed branch"
///
/// The handle is checked first, so a sealed branch gone stale is `HandleInvalid`;
/// a repeated seal returns its result "after checking the head again".
#[test]
fn a_sealed_branch_gone_stale_is_handle_invalid() {
    let db = TestDb::new();
    assert_err!(db.branch_info(sealed_stale(&db)), ApiError::HandleInvalid);
    assert_err!(db.checkpoint(sealed_stale(&db)), ApiError::HandleInvalid);
    assert_err!(db.rollback(sealed_stale(&db)), ApiError::HandleInvalid);
    assert_err!(db.seal(sealed_stale(&db)), ApiError::HandleInvalid);
    assert_err!(db.discard(sealed_stale(&db)), ApiError::HandleInvalid);
    let write = RecordOp::create(key(1), B).field("a", CellValue::from_u32(1));
    assert_err!(
        db.create(sealed_stale(&db), write).result,
        ApiError::HandleInvalid
    );
    let target = ReadTarget::Branch(sealed_stale(&db));
    assert_err!(
        db.get(target, RecordOp::get(key(1), B)).result,
        ApiError::HandleInvalid
    );
}

/// "`NoFrameToRollback` | rollback with every frame already rolled back"
/// "1. Handle: unknown or consumed handle, or stale origin → `HandleInvalid`;
/// sealed → `Sealed`": before the rollback finds no frame.
#[test]
fn rolling_back_every_frame_then_once_more_is_no_frame_to_rollback() {
    let db = TestDb::new();
    // A branch with no writes has one frame: its first rollback succeeds.
    let fresh = db.begin().unwrap();
    db.rollback(fresh).unwrap();
    assert_err!(db.rollback(fresh), ApiError::NoFrameToRollback);
    db.seal(fresh).unwrap();
    assert_err!(db.rollback(fresh), ApiError::Sealed);
    let spent = db.begin().unwrap();
    db.rollback(spent).unwrap();
    db.commit(db.begin().unwrap()).unwrap();
    assert_err!(db.rollback(spent), ApiError::HandleInvalid);
    let branch = db.begin().unwrap();
    let write = |n| {
        db.create(
            branch,
            RecordOp::create(key(n), B).field("a", CellValue::from_u32(1)),
        )
        .result
        .unwrap();
    };
    write(1);
    db.checkpoint(branch).unwrap();
    write(2);
    db.rollback(branch).unwrap();
    db.rollback(branch).unwrap();
    let version = db.branch_info(branch).unwrap().version;
    assert_err!(db.rollback(branch), ApiError::NoFrameToRollback);
    // The error leaves the branch as it was, and usable.
    assert_eq!(db.branch_info(branch).unwrap().version, version);
    assert_err!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(key(1), B))
            .result,
        ApiError::NotFound
    );
    write(3);
    db.commit(branch).unwrap();
}

/// "`StoreFull` | the store reached its size cap; the commit wrote nothing."
/// "A storage failure after sealing keeps the sealed result for a retry or
/// discard. A full store is `StoreFull`; the head stays unchanged."
#[test]
fn a_full_store_is_store_full_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let options = MdbxOptions {
        max_map_size: 1024 * 1024,
        ..MdbxOptions::default()
    };
    let db = Database::open_with_options(
        dir.path(),
        &OpenConfig::new(genesis(LIMITS, PricingConfig::zero())).with_mode(OpenMode::CreateNew),
        options,
    )
    .unwrap();
    // Commits of records full of the largest values, until one does not fit.
    let value = CellValue::from_bytes(&[7; LIMITS.max_bytes_len as usize]);
    let mut keys = (0u32..).map(|n| {
        let mut k = [1; 32];
        k[..4].copy_from_slice(&n.to_be_bytes());
        RecordKey(k)
    });
    let mut fill = |branch| {
        let batch: Vec<RecordKey> = keys.by_ref().take(16).collect();
        for &k in &batch {
            let op = (0..20).fold(RecordOp::create(k, B), |op, i| {
                op.field(format!("f{i}").as_str(), value.clone())
            });
            db.create(branch, op).result.unwrap();
        }
        batch[0]
    };
    let alloc = || {
        db.get(ReadTarget::Head, RecordOp::get(reserved_key("#alloc"), B))
            .result
            .unwrap()
    };
    let mut failure = None;
    for _ in 0..1000 {
        let (head, before) = (db.head().unwrap(), alloc());
        let (branch, bystander) = (db.begin().unwrap(), db.begin().unwrap());
        let first = fill(branch);
        let sealed = db.seal(branch).unwrap();
        if let Err(error) = db.commit(branch) {
            failure = Some((error, head, before, branch, sealed, bystander, first));
            break;
        }
    }
    let (error, head, before, branch, sealed, bystander, first) =
        failure.expect("store never filled");
    assert!(matches!(error, ApiError::StoreFull), "{error:?}");
    assert_eq!(db.head().unwrap(), head);
    assert_eq!(alloc(), before);
    assert_err!(
        db.get(ReadTarget::Head, RecordOp::get(first, B)).result,
        ApiError::NotFound
    );
    // The branch keeps its sealed result: a retry finds the store still full.
    assert_eq!(db.seal(branch).unwrap(), sealed);
    assert_err!(db.commit(branch), ApiError::StoreFull);
    assert_eq!(db.head().unwrap(), head);
    db.discard(branch).unwrap();
    // A branch begun over the unchanged head is not stale.
    db.branch_info(bystander).unwrap();
    // Committed without a seal first, it seals itself and keeps that too.
    fill(bystander);
    assert_err!(db.commit(bystander), ApiError::StoreFull);
    assert_eq!(db.head().unwrap(), head);
    assert!(db.branch_info(bystander).unwrap().sealed);
}
