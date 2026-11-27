//! Receipts.
//!
//! Where the specs disagree, the tests follow the code:
//! - `cost`, budgets and the ledger: the API spec's Receipts section says `cost` is
//!   always 0, `OutOfBudget` is never returned and the ledger is not implemented,
//!   while its Order-of-checks table charges admission and state reads. The code
//!   meters every call, enforces the budget and fills the ledger as the metering spec
//!   defines them; under the zero pricing and unlimited budget of `TestDb::new()`
//!   and `B`, every cost is 0. Tested under real prices: bad handles and unavailable
//!   commits cost 0, admission and record state are charged, a plan-phase failure
//!   is charged no writes, the cost is the priced ledger, and `OutOfBudget` receipts.
//! - The cost of `CommitUnavailable`: the API spec checks the target at no cost; the
//!   metering spec charges admission and `read.base`. The code charges nothing.
//! - A builder error under a budget that cannot pay for admission: the API spec
//!   reports builder errors as `InvalidArgument`; the metering spec charges rejected
//!   input, so the code returns `OutOfBudget`.
//!
//! Where the specs disagree and the code may change, so nothing here tests it:
//! - `priced_at` on a sealed or stale branch: the Receipts table gives `None` only
//!   for an unknown or consumed handle, and the origin otherwise; the metering spec
//!   says a stale or sealed handle captures no pricing snapshot. The code gives
//!   `None` for a write on a sealed branch and for any call on a stale one. Open
//!   until the branch API settles.
//!
//! Not reachable through the public API:
//! - a failed head read (`priced_at: None`): needs a broken store;
//! - "priced at the origin" apart from "priced at the head": any commit makes every
//!   open branch stale, so a live branch's origin is always the head;
//! - a weight change applying to reads of historical data from the next head on:
//!   the API cannot change weights (`@modelWeight` is reserved).

use golemdb_conformance::*;

/// `key(1)` with `a`, a `u32` attribute 1, and `b`, a `str` field "x", committed,
/// at `pricing`.
fn setup(pricing: PricingConfig) -> TestDb {
    let db = TestDb::with_pricing(pricing);
    let branch = db.begin().unwrap();
    db.create(branch, op_create_1()).result.unwrap();
    db.commit(branch).unwrap();
    db
}

/// The create of [`setup`]'s record.
fn op_create_1() -> RecordOp<op::Create> {
    RecordOp::create(key(1), B)
        .attribute("a", CellValue::from_u32(1))
        .field("b", CellValue::from_str("x"))
}

/// A read of `key(1)` and of the missing `key(9)` at `target`, after [`setup`].
fn reads_1_9(db: &TestDb, target: ReadTarget) -> Vec<Metered<()>> {
    [key(1), key(9)]
        .map(|k| db.get(target, RecordOp::get(k, B)).map(|_| ()))
        .into()
}

/// A write of each kind on `branch`, after [`setup`]. On a live branch the first
/// three succeed; the rest fail at admission, or at record state, the last after
/// planning an update and a delete.
fn writes(db: &TestDb, branch: BranchId) -> Vec<Metered<()>> {
    let v = || CellValue::from_u32(1);
    let patch = || RecordOp::patch(key(1), B);
    vec![
        db.create(branch, RecordOp::create(key(2), B).field("c", v()))
            .map(|_| ()),
        db.patch(branch, patch().field("b", v())),
        db.delete(branch, RecordOp::delete(key(2), B)),
        // Admission: a reserved key, a bad name, an over-long value.
        db.create(branch, RecordOp::create(reserved_key("#alloc"), B))
            .map(|_| ()),
        db.delete(branch, RecordOp::delete(reserved_key("#alloc"), B)),
        db.create(branch, RecordOp::create(key(3), B).field("x y", v()))
            .map(|_| ()),
        db.patch(
            branch,
            patch().field("s", CellValue::from_str(&"a".repeat(65))),
        ),
        // Record state: `AlreadyExists`, `NotFound`, `CellNotFound`.
        db.create(branch, op_create_1()).map(|_| ()),
        db.patch(branch, RecordOp::patch(key(9), B).field("a", v())),
        db.delete(branch, RecordOp::delete(key(9), B)),
        db.patch(branch, patch().set("z", v())),
        db.patch(
            branch,
            patch()
                .field("b", CellValue::from_u64(7))
                .remove("a")
                .set("z", v()),
        ),
    ]
}

/// The receipt's ledger priced by `pricing`: each count times its weight's price.
fn priced_ledger(receipt: &Receipt, pricing: &PricingConfig) -> u64 {
    receipt
        .ledger
        .iter()
        .map(|(name, count)| count * pricing.weights[name.as_str()])
        .sum()
}

/// Asserts that `receipt` charges none of the cell and index writes.
#[track_caller]
fn charges_no_writes(receipt: &Receipt) {
    for weight in [
        WeightName::WRITE_CELL_CREATE,
        WeightName::WRITE_CELL_UPDATE,
        WeightName::WRITE_CELL_DELETE,
        WeightName::WRITE_CELL_TRIE_UPDATE,
        WeightName::WRITE_CELL_BYTE,
        WeightName::WRITE_INDEX_JOIN,
        WeightName::WRITE_INDEX_LEAVE,
        WeightName::WRITE_INDEX_TERM_CREATE,
        WeightName::WRITE_INDEX_TRIE_UPDATE,
        WeightName::WRITE_INDEX_BYTE,
    ] {
        assert_eq!(receipt.ledger.count(weight.as_str()), 0, "{weight}");
    }
}

/// Every weight priced apart: 1, 2, 3, … in name order.
fn pricing() -> PricingConfig {
    let mut pricing = PricingConfig::zero();
    for (price, weight) in (1..).zip(pricing.weights.values_mut()) {
        *weight = price;
    }
    pricing
}

/// "`Metered::new(result, receipt)`; `Metered::unmetered(result, priced_at)` for
/// mocks; `into_result()` drops the receipt."
#[test]
fn metered_constructors_keep_the_result() {
    let metered = Metered::unmetered(Ok(7), Some(CommitId::new(3)));
    assert_eq!(
        (
            metered.receipt.cost,
            metered.receipt.priced_at,
            metered.receipt.details
        ),
        (0, Some(CommitId::new(3)), Details::default())
    );
    assert_eq!(metered.into_result().unwrap(), 7);
    let failed = Metered::<()>::unmetered(Err(ApiError::NotFound), None);
    assert_err!(failed.into_result(), ApiError::NotFound);
    // `new` keeps the receipt given: a real one, with non-zero details.
    let db = TestDb::new();
    let receipt = db.create(db.begin().unwrap(), op_create_1()).receipt;
    let metered = Metered::new(Ok(7), receipt.clone());
    assert_eq!(metered.receipt, receipt);
    assert_eq!(metered.into_result().unwrap(), 7);
}

/// "| 1. Handle | … | 0 |"
/// "**Bad handles cost 0.** A call on an unknown, consumed, stale or sealed handle is
/// rejected before admission and charged nothing (metering D8)."
/// "Order of checks: the target first, at no cost (an invalid branch handle →
/// `HandleInvalid`; an unavailable commit → `CommitUnavailable`)"
/// "| unknown or consumed handle; failed head read | `None`: no snapshot was
/// captured, and the call costs 0 |"
/// "Failed calls and reads report all zeros."
#[test]
fn rejected_handles_and_targets_cost_and_count_nothing() {
    let db = setup(pricing());
    let head = db.head().unwrap();
    let mut rejected = Vec::new();
    let mut reject = |calls: Vec<Metered<()>>, error: fn(&ApiError) -> bool| {
        for call in calls {
            assert!(call.result.as_ref().is_err_and(error));
            rejected.push(call.receipt);
        }
    };
    // Unavailable commits.
    for commit in [0, head.get() + 1] {
        reject(
            reads_1_9(&db, ReadTarget::Commit(CommitId::new(commit))),
            |error| matches!(error, ApiError::CommitUnavailable { .. }),
        );
    }
    // Writes on a sealed branch, before any commit makes it stale.
    let sealed = db.begin().unwrap();
    db.seal(sealed).unwrap();
    reject(writes(&db, sealed), |error| {
        matches!(error, ApiError::Sealed)
    });
    // Any call on a stale branch, and on a consumed or unknown one, which also has
    // no price.
    let calls = |branch| {
        let mut calls = writes(&db, branch);
        calls.extend(reads_1_9(&db, ReadTarget::Branch(branch)));
        calls
    };
    let stale = db.begin().unwrap();
    db.commit(db.begin().unwrap()).unwrap();
    reject(calls(stale), |error| {
        matches!(error, ApiError::HandleInvalid)
    });
    let committed = db.begin().unwrap();
    db.commit(committed).unwrap();
    let discarded = db.begin().unwrap();
    db.discard(discarded).unwrap();
    for branch in [committed, discarded, BranchId::new(u64::MAX)] {
        let calls = calls(branch);
        assert!(calls.iter().all(|call| call.receipt.priced_at.is_none()));
        reject(calls, |error| matches!(error, ApiError::HandleInvalid));
    }
    for receipt in &rejected {
        assert_eq!(
            (receipt.cost, receipt.details, receipt.ledger.is_empty()),
            (0, Details::default(), true)
        );
    }
}

/// "| 2. Admission | … | admission |"
/// "| 3. Record state | … | state reads |"
#[test]
fn valid_calls_are_charged() {
    let db = setup(pricing());
    let head = db.head().unwrap();
    let branch = db.begin().unwrap();
    let sealed = db.begin().unwrap();
    db.seal(sealed).unwrap();
    let mut charged = writes(&db, branch);
    assert!(charged[..3].iter().all(|call| call.result.is_ok()));
    assert!(charged[3..].iter().all(|call| call.result.is_err()));
    // Reads found or not, of the head and of a branch, sealed or not.
    for target in [
        ReadTarget::Head,
        ReadTarget::Commit(head),
        ReadTarget::Branch(branch),
        ReadTarget::Branch(sealed),
    ] {
        charged.extend(reads_1_9(&db, target));
    }
    for call in &charged {
        assert!(call.receipt.cost > 0);
        assert_eq!(call.receipt.cost, priced_ledger(&call.receipt, &pricing()));
    }
    // A failure in the plan phase is charged no writes it planned.
    let planned = db.patch(
        branch,
        RecordOp::patch(key(1), B)
            .field("b", CellValue::from_u64(7))
            .remove("a")
            .set("z", CellValue::from_u32(1)),
    );
    assert_err!(planned.result, ApiError::CellNotFound { .. });
    charges_no_writes(&planned.receipt);
    // Admission reads no record state.
    for rejected in [
        db.create(branch, RecordOp::create(reserved_key("#alloc"), B))
            .map(|_| ()),
        db.patch(
            branch,
            RecordOp::patch(key(1), B).field("x y", CellValue::from_u32(1)),
        ),
    ] {
        assert!(rejected.result.is_err());
        assert!(
            rejected
                .receipt
                .ledger
                .iter()
                .all(|(name, _)| !name.as_str().starts_with("read."))
        );
    }
}

/// "| write, or `get` on a branch | the branch's origin commit, looked up without side
/// effects |"
#[test]
fn writes_and_branch_reads_are_priced_at_the_origin() {
    let db = setup(PricingConfig::zero());
    let branch = db.begin().unwrap();
    let origin = Some(db.branch_info(branch).unwrap().commit_id);
    // On success and on failure, and reads of a sealed branch, which stays readable.
    let sealed = db.begin().unwrap();
    db.seal(sealed).unwrap();
    let mut all = writes(&db, branch);
    all.extend(reads_1_9(&db, ReadTarget::Branch(branch)));
    all.extend(reads_1_9(&db, ReadTarget::Branch(sealed)));
    assert!(all.iter().all(|call| call.receipt.priced_at == origin));
    // Looking it up left the branch valid.
    db.commit(branch).unwrap();
}

/// "| `get` on `Head` | the head of the snapshot read |"
/// "| `get` on `Commit(c)`, success or failure | the head at admission, not `c`: reads
/// of old data use current prices |"
#[test]
fn committed_reads_are_priced_at_the_head() {
    let db = setup(PricingConfig::zero());
    let first = db.head().unwrap();
    // Before and after another commit: the current head, not a cached one.
    for _ in 0..2 {
        let head = db.head().unwrap();
        // Found or not, at the head or at a commit that is unavailable, before or
        // after it.
        for (target, k) in [
            (ReadTarget::Head, key(1)),
            (ReadTarget::Head, key(9)),
            (ReadTarget::Commit(head), key(1)),
            (ReadTarget::Commit(first), key(1)),
            (ReadTarget::Commit(CommitId::new(0)), key(1)),
            (ReadTarget::Commit(CommitId::new(head.get() + 1)), key(1)),
        ] {
            assert_eq!(
                db.get(target, RecordOp::get(k, B)).receipt.priced_at,
                Some(head)
            );
        }
        db.commit(db.begin().unwrap()).unwrap();
    }
}

/// "| create a cell | `cells_created`; `cell_bytes_written` += its size; if an
/// attribute, `index_joins` and `index_bytes_written` |"
/// "System cells (`#key`, `#meta`, the binding) are not counted."
/// "`#meta` and the receipt's details come from the same computed effects."
#[test]
fn create_counts_each_cell() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    let create = || {
        RecordOp::create(key(1), B)
            .attribute("abc", CellValue::from_u32(1))
            .field("b", CellValue::from_str("x"))
    };
    let created = db.create(branch, create());
    created.result.unwrap();
    let d = created.receipt.details;
    // (8 + |name|) + (1 + |value|): 16 for `abc`, 11 for `b`; `abc`'s term is
    // |name| + 2 + |value| = 9. Nothing else.
    let mut expected = Details::default();
    expected.cells_created = 2;
    expected.cell_bytes_written = 16 + 11;
    expected.index_joins = 1;
    expected.index_bytes_written = 9;
    assert_eq!(d, expected);
    // `#meta` counts the same.
    assert_eq!(
        db.read(ReadTarget::Branch(branch), key(1)).meta(),
        Some(RecordMeta {
            cells: d.cells_created,
            cell_bytes: d.cell_bytes_written,
            indexed_cells: d.index_joins,
            index_bytes: d.index_bytes_written,
        })
    );
    // Deleted and re-created on the same branch, its cells count as created again,
    // not as updates of the old ones.
    db.delete(branch, RecordOp::delete(key(1), B))
        .result
        .unwrap();
    let recreated = db.create(branch, create());
    recreated.result.unwrap();
    assert_eq!(recreated.receipt.details, d);
    // An empty record counts nothing.
    let created = db.create(branch, RecordOp::create(key(2), B));
    created.result.unwrap();
    assert_eq!(created.receipt.details, Details::default());
}

/// "| create a cell | `cells_created`; `cell_bytes_written` += its size; if an
/// attribute, `index_joins` and `index_bytes_written` |"
/// "| update a cell | `cells_updated`; old size to `cell_bytes_deleted`, new size to
/// `cell_bytes_written`; leave for an old attribute, join for a new one |"
/// "| delete a cell | `cells_deleted`; `cell_bytes_deleted`; if an attribute,
/// `index_leaves` and `index_bytes_deleted` |"
/// "Effects, not requests: a no-op counts nothing."
/// "System cells (`#key`, `#meta`, the binding) are not counted."
#[test]
fn patch_counts_each_change_by_size() {
    let db = setup(PricingConfig::zero());
    // Cells are (8 + |name|) + (1 + |value|), terms |name| + 2 + |value|.
    let (a, a_term) = (14, 7); // `a` as a `u32`: (8 + 1) + (1 + 4), 1 + 2 + 4
    let (a64, a64_term) = (18, 11); // `a` as a `u64`
    let (b, b_term) = (11, 4); // `b` = "x"
    let (c, c_term) = (12, 5); // `c` = "yy"
    // Each patch on its own branch, and what it counts:
    // - cells created, updated and deleted;
    // - cell bytes written and deleted;
    // - index joins and leaves;
    // - index bytes written and deleted.
    // The record is committed, or created on the patching
    // branch, so the patch reads the old values from the store or from the branch.
    let patch = || RecordOp::patch(key(1), B);
    for (op, cells, bytes, terms, term_bytes) in [
        // `#meta` is rewritten too, uncounted.
        (
            patch()
                .attribute("a", CellValue::from_u32(2))
                .field("c", CellValue::from_str("yy"))
                .remove("b"),
            (1, 1, 1),
            (a + c, a + b),
            (1, 1),
            (a_term, a_term),
        ),
        (
            patch().attribute("c", CellValue::from_str("yy")),
            (1, 0, 0),
            (c, 0),
            (1, 0),
            (c_term, 0),
        ),
        (patch().remove("a"), (0, 0, 1), (0, a), (0, 1), (0, a_term)),
        // Kind flips: a leave only, a join only.
        (
            patch().field("a", CellValue::from_u32(1)),
            (0, 1, 0),
            (a, a),
            (0, 1),
            (0, a_term),
        ),
        (
            patch().attribute("b", CellValue::from_str("x")),
            (0, 1, 0),
            (b, b),
            (1, 0),
            (b_term, 0),
        ),
        // `set` keeps the stored kind.
        (
            patch().set("a", CellValue::from_u64(2)),
            (0, 1, 0),
            (a64, a),
            (1, 1),
            (a64_term, a_term),
        ),
        // An equal write and a remove of a missing cell count nothing; only `c` does.
        (
            patch()
                .attribute("a", CellValue::from_u32(1))
                .field("c", CellValue::from_str("yy"))
                .remove("z"),
            (1, 0, 0),
            (c, 0),
            (0, 0),
            (0, 0),
        ),
    ] {
        let fresh = TestDb::new();
        let created = fresh.begin().unwrap();
        fresh.create(created, op_create_1()).result.unwrap();
        for (db, branch) in [(&db, db.begin().unwrap()), (&fresh, created)] {
            let patched = db.patch(branch, op.clone());
            patched.result.unwrap();
            let d = patched.receipt.details;
            assert_eq!(
                (
                    (d.cells_created, d.cells_updated, d.cells_deleted),
                    (d.cell_bytes_written, d.cell_bytes_deleted),
                    (d.index_joins, d.index_leaves),
                    (d.index_bytes_written, d.index_bytes_deleted)
                ),
                (cells, bytes, terms, term_bytes)
            );
        }
    }
}

/// "| delete a cell | `cells_deleted`; `cell_bytes_deleted`; if an attribute,
/// `index_leaves` and `index_bytes_deleted` |"
/// "System cells (`#key`, `#meta`, the binding) are not counted."
/// "`#meta` and the receipt's details come from the same computed effects."
#[test]
fn delete_counts_what_meta_counted() {
    let db = setup(PricingConfig::zero());
    // The committed record untouched, patched on the deleting branch, or re-created
    // there; and one left empty, which counts nothing.
    let cases: [fn(&TestDb, BranchId); 4] = [
        |_, _| {},
        |db, branch| {
            db.patch(
                branch,
                RecordOp::patch(key(1), B)
                    .attribute("a", CellValue::from_u32(2))
                    .remove("b")
                    .attribute("c", CellValue::from_str("yy")),
            )
            .result
            .unwrap();
        },
        |db, branch| {
            db.delete(branch, RecordOp::delete(key(1), B))
                .result
                .unwrap();
            db.create(
                branch,
                RecordOp::create(key(1), B)
                    .attribute("a", CellValue::from_u32(3))
                    .field("d", CellValue::from_bytes(b"abc")),
            )
            .result
            .unwrap();
        },
        |db, branch| {
            db.patch(branch, RecordOp::patch(key(1), B).remove("a").remove("b"))
                .result
                .unwrap();
        },
    ];
    let mut counted = Vec::new();
    for prepare in cases {
        let branch = db.begin().unwrap();
        prepare(&db, branch);
        let meta = db.read(ReadTarget::Branch(branch), key(1)).meta().unwrap();
        let deleted = db.delete(branch, RecordOp::delete(key(1), B));
        deleted.result.unwrap();
        // What `#meta` counted, and nothing written.
        let mut expected = Details::default();
        expected.cells_deleted = meta.cells;
        expected.cell_bytes_deleted = meta.cell_bytes;
        expected.index_leaves = meta.indexed_cells;
        expected.index_bytes_deleted = meta.index_bytes;
        assert_eq!(deleted.receipt.details, expected);
        counted.push(meta);
    }
    // The untouched record: `a` 14 and `b` 11 bytes, `a`'s term 7; the emptied one
    // nothing.
    assert_eq!(
        counted[0],
        RecordMeta {
            cells: 2,
            cell_bytes: 14 + 11,
            indexed_cells: 1,
            index_bytes: 7
        }
    );
    assert_eq!(counted[3], RecordMeta::default());
}

/// "| create a cell | `cells_created`; `cell_bytes_written` += its size; if an
/// attribute, `index_joins` and `index_bytes_written` |"
/// "| delete a cell | `cells_deleted`; `cell_bytes_deleted`; if an attribute,
/// `index_leaves` and `index_bytes_deleted` |"
/// ""Index terms created" is not reported yet"
#[test]
fn a_shared_index_term_counts_per_cell() {
    let db = setup(PricingConfig::zero());
    let branch = db.begin().unwrap();
    let terms = |d: Details| {
        (
            d.index_joins,
            d.index_leaves,
            d.index_bytes_written,
            d.index_bytes_deleted,
        )
    };
    // `key(2)` joins the term `a = 1` that `key(1)` already holds.
    let created = db.create(
        branch,
        RecordOp::create(key(2), B).attribute("a", CellValue::from_u32(1)),
    );
    created.result.unwrap();
    assert_eq!(terms(created.receipt.details), (1, 0, 7, 0));
    // Deleting `key(1)` leaves it, though `key(2)` still holds it.
    let deleted = db.delete(branch, RecordOp::delete(key(1), B));
    deleted.result.unwrap();
    assert_eq!(terms(deleted.receipt.details), (0, 1, 0, 7));
}

/// "Failed calls and reads report all zeros."
#[test]
fn failed_calls_and_reads_on_a_valid_handle_count_nothing() {
    let db = setup(PricingConfig::zero());
    let branch = db.begin().unwrap();
    let head = db.head().unwrap();
    let sealed = db.begin().unwrap();
    db.seal(sealed).unwrap();
    // Reads, found or not, on every target, and a projection.
    let mut all = vec![
        db.get(ReadTarget::Head, RecordOp::get(key(1), B).only(["a"]))
            .map(|_| ()),
    ];
    for target in [
        ReadTarget::Head,
        ReadTarget::Commit(head),
        ReadTarget::Commit(CommitId::new(0)),
        ReadTarget::Commit(CommitId::new(head.get() + 1)),
        ReadTarget::Branch(branch),
        ReadTarget::Branch(sealed),
    ] {
        all.extend(reads_1_9(&db, target));
    }
    // Writes that fail at admission or at record state.
    all.extend(
        writes(&db, branch)
            .into_iter()
            .filter(|call| call.result.is_err()),
    );
    assert!(
        all.iter()
            .all(|call| call.receipt.details == Details::default())
    );
}

/// "| 1. Handle | … | 0 |"
/// "Order of checks: the target first, at no cost (an invalid branch handle →
/// `HandleInvalid`; an unavailable commit → `CommitUnavailable`)"
#[test]
fn handles_and_targets_are_checked_before_the_budget() {
    let db = setup(pricing());
    let nothing = Budget::Limited(0);
    let v = || CellValue::from_u32(1);
    let sealed = db.begin().unwrap();
    db.seal(sealed).unwrap();
    assert_err!(
        db.patch(sealed, RecordOp::patch(key(1), nothing).field("b", v()))
            .result,
        ApiError::Sealed
    );
    assert_err!(
        db.delete(BranchId::new(u64::MAX), RecordOp::delete(key(1), nothing))
            .result,
        ApiError::HandleInvalid
    );
    assert_err!(
        db.get(
            ReadTarget::Commit(CommitId::new(0)),
            RecordOp::get(key(1), nothing)
        )
        .result,
        ApiError::CommitUnavailable { .. }
    );
    // Admission is charged: a budget that cannot pay for it stops a builder error.
    assert_err!(
        db.patch(
            db.begin().unwrap(),
            RecordOp::patch(key(1), nothing).field("x y", v())
        )
        .result,
        ApiError::OutOfBudget { required: None }
    );
}

/// "`OutOfBudget` exists and is never returned yet; when it is, the amount spent is
/// the receipt's `cost`, and the error carries `required`"
/// "Failed calls and reads report all zeros."
#[test]
fn out_of_budget_counts_nothing_and_spends_within_the_budget() {
    let db = setup(pricing());
    let origin = |branch| Some(db.branch_info(branch).unwrap().commit_id);
    let state = |branch| {
        (
            db.version(branch),
            db.read(ReadTarget::Branch(branch), key(1)),
            db.record_id(branch, key(5)),
            db.next_record_id(branch),
        )
    };
    // Checks an `OutOfBudget` failure at `limit`, short of the call's `full` cost.
    let out_of_budget =
        |metered: Metered<()>, limit: u64, full: u64, priced_at: Option<CommitId>, write: bool| {
            let required = match metered.result {
                Err(ApiError::OutOfBudget { required }) => required,
                Err(error) => panic!("expected OutOfBudget, got {error:?}"),
                Ok(_) => panic!("expected OutOfBudget, got Ok"),
            };
            // A write reports the full cost once planning has established it, as one
            // short of it does; a read stops as it goes and reports none.
            let one_short = limit + 1 == full;
            match (write, one_short) {
                (true, true) => assert_eq!(required, Some(full)),
                (true, false) => assert!(required.is_none_or(|required| required == full)),
                (false, _) => assert_eq!(required, None),
            }
            // What was spent, within the budget and as the ledger prices it; one short
            // of it, something was, but none of the planned writes.
            let receipt = metered.receipt;
            assert!(receipt.cost <= limit);
            assert!(!one_short || receipt.cost > 0);
            assert_eq!(receipt.cost, priced_ledger(&receipt, &pricing()));
            charges_no_writes(&receipt);
            assert_eq!(
                (receipt.priced_at, receipt.details),
                (priced_at, Details::default())
            );
        };
    // A patch, which drops the changes it worked out, a create and a delete: nothing
    // changes.
    let limited: [&dyn Fn(BranchId, Budget) -> Metered<()>; 3] = [
        &|branch, budget| {
            db.patch(
                branch,
                RecordOp::patch(key(1), budget)
                    .attribute("a", CellValue::from_u32(2))
                    .remove("b")
                    .field("c", CellValue::from_str("yy")),
            )
        },
        &|branch, budget| {
            db.create(
                branch,
                RecordOp::create(key(5), budget)
                    .attribute("a", CellValue::from_u32(1))
                    .field("b", CellValue::from_str("x")),
            )
            .map(|_| ())
        },
        &|branch, budget| db.delete(branch, RecordOp::delete(key(1), budget)),
    ];
    for write in limited {
        let full = write(db.begin().unwrap(), B).receipt.cost;
        for limit in [0, full / 2, full - 1] {
            let branch = db.begin().unwrap();
            let before = state(branch);
            let failed = write(branch, Budget::Limited(limit));
            out_of_budget(failed, limit, full, origin(branch), true);
            assert_eq!(state(branch), before);
        }
        // The full cost is enough.
        write(db.begin().unwrap(), Budget::Limited(full))
            .result
            .unwrap();
    }
    // Reads at the head, at the head's commit and on a branch.
    let head = db.head().unwrap();
    let branch = db.begin().unwrap();
    for (target, priced_at) in [
        (ReadTarget::Head, Some(head)),
        (ReadTarget::Commit(head), Some(head)),
        (ReadTarget::Branch(branch), origin(branch)),
    ] {
        let get = |budget| db.get(target, RecordOp::get(key(1), budget)).map(|_| ());
        let full = get(B).receipt.cost;
        for limit in [0, full / 2, full - 1] {
            out_of_budget(get(Budget::Limited(limit)), limit, full, priced_at, false);
        }
        get(Budget::Limited(full)).result.unwrap();
    }
}

/// "| cost arithmetic overflows `u64` | completed steps | `None` |"
#[test]
fn an_overflowing_cost_is_out_of_budget_even_unlimited() {
    let overflowing = |weight: WeightName| {
        let mut pricing = pricing();
        pricing.weights.insert(weight.as_str().into(), u64::MAX);
        pricing
    };
    // A cell byte at `u64::MAX`: a create overflows and writes nothing.
    let pricing = overflowing(WeightName::WRITE_CELL_BYTE);
    let db = TestDb::with_pricing(pricing.clone());
    let branch = db.begin().unwrap();
    let created = db.create(branch, op_create_1());
    assert_err!(created.result, ApiError::OutOfBudget { required: None });
    assert_eq!(db.record_id(branch, key(1)), None);
    // The completed steps, counting nothing.
    assert_eq!(
        created.receipt.cost,
        priced_ledger(&created.receipt, &pricing)
    );
    assert_eq!(created.receipt.details, Details::default());

    // A read byte at `u64::MAX`: a read overflows.
    let pricing = overflowing(WeightName::READ_BYTE);
    let db = setup(pricing.clone());
    let read = db.get(ReadTarget::Head, RecordOp::get(key(1), B));
    assert_err!(read.result, ApiError::OutOfBudget { required: None });
    assert_eq!(read.receipt.cost, priced_ledger(&read.receipt, &pricing));
}
