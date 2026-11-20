//! Record operations: `patch`.
//!
//! Where the code breaks the spec, the test is `#[ignore]`d with the reason.
//!
//! Agreed changes the code already implements, tested as agreed:
//! - `.set(n, v)` and `CellNotFound { name }`;
//! - admission before record state: the spec says `patch` checks the limits after
//!   `NotFound` today; the code checks them before;
//! - the handle before admission: the spec says the implementations check reserved
//!   keys before the handle; the code checks the handle first.
//!
//! Where the spec is stale or silent, so nothing here tests it:
//! - Per-record caps: the spec lists `#maxRecordCells` and `#maxRecordIndexedCells`
//!   as not implemented; the code enforces them with `InvalidArgument`.
//! - `RecordOp::validate()`, `changes()`, `value()` and `removes()` are in the spec,
//!   not the code; the code has `entries()` instead of `changes()`, in caller order
//!   with duplicates kept.
//! - `set` with a value that cannot take the stored kind (`bytes` onto an
//!   attribute): the spec is silent; the code returns `InvalidArgument` at record
//!   state, with the name in debug form, unlike every other message:
//!   `set(CellName([97])): invalid cell value for stored kind`.
//! - Reserved-looking keys: whether a zero-padded `#…` key outside the catalogue,
//!   or the all-zero key, is reserved is open; the code treats both as user keys,
//!   so a write to one that is not bound is `NotFound`.
//!
//! Not reachable through the public API:
//! - a user record without `#meta`, or a count falling below zero (`Internal`): both
//!   need corrupted state.

use golemdb_conformance::*;

/// `key(1)` with `a`, a `u32` attribute 1, and `b`, a `str` field "x", committed;
/// and a fresh branch.
fn setup() -> (TestDb, BranchId) {
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
    let branch = db.begin().unwrap();
    (db, branch)
}

/// The branch's `version` and its read of `key(1)`.
fn state(db: &TestDb, branch: BranchId) -> (u64, Record) {
    (
        db.version(branch),
        db.read(ReadTarget::Branch(branch), key(1)),
    )
}

/// A patch that admission rejects twice over: a reserved key and a bad name.
fn bad_everywhere() -> RecordOp<op::Patch> {
    RecordOp::patch(reserved_key("#alloc"), B).field("x y", CellValue::from_u32(1))
}

/// "`#` and `@` start reserved names; a user name can start with neither."
/// "| a name violating the grammar | `attribute("x y"): invalid cell name`, cause as
/// source |"
#[test]
fn reserved_cell_names_cannot_be_patched() {
    let (db, branch) = setup();
    let before = db.read(ReadTarget::Branch(branch), key(1));
    let patch = || RecordOp::patch(key(1), B);
    let u32 = || CellValue::from_u32(1);
    // `#key`, `#meta` and a name `@` starts, by every step.
    for name in ["#key", "#meta", "@x"] {
        for op in [
            patch().remove(name),
            patch().set(name, u32()),
            patch().field(name, u32()),
            patch().attribute(name, u32()),
        ] {
            assert_err!(
                db.patch(branch, op).result,
                ApiError::InvalidArgument { message, source: Some(_) }
                    if message.ends_with(&format!("({name:?}): invalid cell name"))
            );
        }
    }
    // `#key` and `#meta` are intact.
    assert_eq!(db.read(ReadTarget::Branch(branch), key(1)), before);
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
    let (db, branch) = setup();
    let before = state(&db, branch);
    let patch = || RecordOp::patch(key(1), B);
    let v = || CellValue::from_u32(1);
    // A bad name or value carries its cause. Of two problems, the first given is
    // reported, not the first by name.
    for (op, expected) in [
        (patch().remove("x y"), "remove(\"x y\"): invalid cell name"),
        (patch().set("x y", v()), "set(\"x y\"): invalid cell name"),
        (
            patch().attribute("f", CellValue::from_bytes(b"a")),
            "attribute(\"f\"): invalid cell value",
        ),
        (
            patch().field("y z", v()).remove("x y"),
            "field(\"y z\"): invalid cell name",
        ),
        (
            patch().field("y z", v()).field("x", v()).field("x", v()),
            "field(\"y z\"): invalid cell name",
        ),
    ] {
        assert_err!(
            db.patch(branch, op).result,
            ApiError::InvalidArgument { message, source: Some(_) } if message == expected
        );
    }
    // A duplicate names its second step, whichever steps they are, with no cause.
    for (op, second) in [
        (patch().field("x", v()).remove("x"), "remove"),
        (patch().remove("x").remove("x"), "remove"),
        (patch().field("x", v()).field("x", v()), "field"),
        (patch().attribute("x", v()).set("x", v()), "set"),
        (patch().remove("x").field("x", v()), "field"),
        (patch().remove("x").set("x", v()), "set"),
        // Before a later bad name.
        (
            patch().field("x", v()).field("x", v()).field("y z", v()),
            "field",
        ),
    ] {
        let expected =
            format!("{second}(\"x\"): the operation already writes or removes this cell");
        assert_err!(
            db.patch(branch, op).result,
            ApiError::InvalidArgument { message, source: None } if message == expected
        );
    }
    assert_eq!(state(&db, branch), before);
}

/// "1. Handle → `HandleInvalid` / `Sealed`."
#[test]
fn handle_errors_come_first() {
    let db = TestDb::new();
    let bind = db.begin().unwrap();
    db.create(bind, RecordOp::create(key(1), B)).result.unwrap();
    db.commit(bind).unwrap();
    // A patch that admission would reject (a reserved key and a bad name), one that
    // record state would (`key(9)` is not bound), and a valid one.
    let ops = || {
        [
            bad_everywhere(),
            RecordOp::patch(key(9), B),
            RecordOp::patch(key(1), B).field("a", CellValue::from_u32(1)),
        ]
    };
    // On a stale branch: one per op, as the first call that detects one removes it.
    for op in ops() {
        let stale = db.begin().unwrap();
        db.commit(db.begin().unwrap()).unwrap();
        assert_err!(db.patch(stale, op).result, ApiError::HandleInvalid);
    }
    // A sealed branch gone stale is `HandleInvalid` too, not `Sealed`.
    for op in ops() {
        let stale = db.begin().unwrap();
        db.seal(stale).unwrap();
        db.commit(db.begin().unwrap()).unwrap();
        assert_err!(db.patch(stale, op).result, ApiError::HandleInvalid);
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
            assert_err!(db.patch(branch, op).result, ApiError::HandleInvalid);
        }
    }
    // On a sealed branch.
    for op in ops() {
        assert_err!(db.patch(sealed, op).result, ApiError::Sealed);
    }
}

/// "2. Admission: builder error → `InvalidArgument`; reserved key → `Reserved`; names
/// and values against the limits → `InvalidArgument`."
#[test]
#[ignore = "admission checks the reserved key before builder errors"]
fn builder_errors_come_before_reserved() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    assert_err!(
        db.patch(branch, bad_everywhere()).result,
        ApiError::InvalidArgument { .. }
    );
}

/// "2. Admission: builder error → `InvalidArgument`; reserved key → `Reserved`; names
/// and values against the limits → `InvalidArgument`."
#[test]
#[ignore = "admission checks an entry's limits before its grammar"]
fn builder_errors_come_before_limits() {
    let (db, branch) = setup();
    let long = || CellValue::from_str(&"a".repeat(65));
    // The bad name on the over-long value itself, or after another over-long value.
    for op in [
        RecordOp::patch(key(1), B).field("a b", long()),
        RecordOp::patch(key(1), B)
            .field("b", long())
            .field("a b", CellValue::from_u32(1)),
    ] {
        assert_err!(
            db.patch(branch, op).result,
            ApiError::InvalidArgument { message, .. } if message == "field(\"a b\"): invalid cell name"
        );
    }
}

/// "2. Admission: builder error → `InvalidArgument`; reserved key → `Reserved`; names
/// and values against the limits → `InvalidArgument`."
/// "| a name written or removed twice in one operation | `remove("x"): the operation
/// already writes or removes this cell` |"
#[test]
#[ignore = "admission checks an entry's limits before duplicates"]
fn duplicates_come_before_limits() {
    let (db, branch) = setup();
    // The duplicate step's value is over-long.
    assert_err!(
        db.patch(
            branch,
            RecordOp::patch(key(1), B)
                .field("x", CellValue::from_u32(1))
                .field("x", CellValue::from_str(&"a".repeat(65)))
        )
        .result,
        ApiError::InvalidArgument { message, .. }
            if message == "field(\"x\"): the operation already writes or removes this cell"
    );
}

/// "2. Admission: builder error → `InvalidArgument`; reserved key → `Reserved`; names
/// and values against the limits → `InvalidArgument`."
#[test]
fn reserved_keys_are_reserved_before_limits() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    for name in RESERVED_NAMES {
        let before = db.read(ReadTarget::Branch(branch), reserved_key(name));
        // An empty patch, a valid write, one with an over-long value, and a `set`
        // of a missing cell, which record state would reject.
        for op in [
            RecordOp::patch(reserved_key(name), B),
            RecordOp::patch(reserved_key(name), B).field("x", CellValue::from_u32(1)),
            RecordOp::patch(reserved_key(name), B).field("s", CellValue::from_str(&"a".repeat(65))),
            RecordOp::patch(reserved_key(name), B).set("zz", CellValue::from_u32(1)),
        ] {
            assert_err!(db.patch(branch, op).result, ApiError::Reserved);
        }
        // The reserved record is unchanged.
        assert_eq!(
            db.read(ReadTarget::Branch(branch), reserved_key(name)),
            before
        );
    }
}

/// "2. Admission: builder error → `InvalidArgument`; reserved key → `Reserved`; names
/// and values against the limits → `InvalidArgument`."
#[test]
fn limits_are_inclusive() {
    let (db, branch) = setup();
    let (name, long_name) = ("n".repeat(32), "n".repeat(33));
    // A name, a `str` and a `bytes` value at their limits are accepted, by every
    // step that takes them.
    db.patch(
        branch,
        RecordOp::patch(key(1), B)
            .field(&name, CellValue::from_u32(1))
            .attribute("s", CellValue::from_str(&"a".repeat(64)))
            .field("c", CellValue::from_bytes(&[0; 128]))
            .set("b", CellValue::from_str(&"a".repeat(64)))
            .remove(&"m".repeat(32)),
    )
    .result
    .unwrap();
    db.patch(
        branch,
        RecordOp::patch(key(1), B)
            .set("c", CellValue::from_bytes(&[1; 128]))
            .set(&name, CellValue::from_u32(2))
            .attribute(&"o".repeat(32), CellValue::from_u32(3)),
    )
    .result
    .unwrap();
    // One byte over each is rejected, also when the cell is removed or missing.
    for op in [
        RecordOp::patch(key(1), B).field(&long_name, CellValue::from_u32(1)),
        RecordOp::patch(key(1), B).remove(&long_name),
        RecordOp::patch(key(1), B).set(&long_name, CellValue::from_u32(1)),
        RecordOp::patch(key(1), B).attribute(&long_name, CellValue::from_u32(1)),
        RecordOp::patch(key(1), B).attribute("s", CellValue::from_str(&"a".repeat(65))),
        RecordOp::patch(key(1), B).set("b", CellValue::from_str(&"a".repeat(65))),
        RecordOp::patch(key(1), B).field("c", CellValue::from_bytes(&[0; 129])),
        RecordOp::patch(key(1), B).set("c", CellValue::from_bytes(&[0; 129])),
    ] {
        assert_err!(
            db.patch(branch, op).result,
            ApiError::InvalidArgument { .. }
        );
    }
}

/// "2. Admission: builder error → `InvalidArgument`; reserved key → `Reserved`; names
/// and values against the limits → `InvalidArgument`."
/// "3. Record state: key not bound → `NotFound`;"
#[test]
fn admission_errors_come_before_not_found() {
    let (db, branch) = setup();
    db.delete(branch, RecordOp::delete(key(1), B))
        .result
        .unwrap();
    db.create(branch, RecordOp::create(key(2), B))
        .result
        .unwrap();
    db.commit(branch).unwrap();
    let branch = db.begin().unwrap();
    db.delete(branch, RecordOp::delete(key(2), B))
        .result
        .unwrap();
    // `key(1)` is deleted in an earlier commit, `key(2)` on this branch, and
    // `key(9)` never existed.
    for k in [key(1), key(2), key(9)] {
        // A builder error, an over-long name and an over-long value fail admission.
        for op in [
            RecordOp::patch(k, B).remove("x y"),
            RecordOp::patch(k, B).remove(&"n".repeat(33)),
            RecordOp::patch(k, B).field("s", CellValue::from_str(&"a".repeat(65))),
        ] {
            assert_err!(
                db.patch(branch, op).result,
                ApiError::InvalidArgument { .. }
            );
        }
        // With valid changes, the missing record is reported.
        assert_err!(
            db.patch(branch, RecordOp::patch(k, B).remove("a")).result,
            ApiError::NotFound
        );
    }
}

/// "3. Record state: key not bound → `NotFound`; then `#meta` and each named cell, in
/// name order, as below."
/// "| missing | `set` (agreed) | **`CellNotFound { name }`**; the whole patch fails and
/// nothing is written |"
#[test]
fn cells_are_checked_in_name_order() {
    let (db, branch) = setup();
    // Two missing cells, the second given first by name: `y` before `z`, and by
    // bytes, `Y` (0x59) before `c` (0x63), though after it alphabetically.
    for (first, second) in [("z", "y"), ("c", "Y")] {
        let v = || CellValue::from_u32(1);
        assert_err!(
            db.patch(branch, RecordOp::patch(key(1), B).set(first, v()).set(second, v()))
                .result,
            ApiError::CellNotFound { name } if name.as_bytes() == second.as_bytes()
        );
    }
    // The binding comes before the cells.
    assert_err!(
        db.patch(
            branch,
            RecordOp::patch(key(9), B).set("z", CellValue::from_u32(1))
        )
        .result,
        ApiError::NotFound
    );
}

/// "| missing | `attribute` / `field` | create |"
/// "| present, different | `attribute` / `field` | update |"
#[test]
fn writes_create_missing_cells_and_update_different_ones() {
    let (db, branch) = setup();
    // A neighbour, the allocator and the binding, which no patch touches.
    db.create(
        branch,
        RecordOp::create(key(2), B).attribute("a", CellValue::from_u32(1)),
    )
    .result
    .unwrap();
    let untouched = || {
        (
            db.read(ReadTarget::Branch(branch), key(2)),
            db.next_record_id(branch),
            db.record_id(branch, key(1)),
        )
    };
    let before = untouched();
    let patched = db.patch(
        branch,
        RecordOp::patch(key(1), B)
            // Missing: created, as either kind.
            .field("c", CellValue::from_u32(3))
            .attribute("d", CellValue::from_u32(4))
            // Present: a new payload, and a new type with the same payload.
            .attribute("a", CellValue::from_u32(2))
            .field("b", CellValue::from_bytes(b"x")),
    );
    patched.result.unwrap();
    assert_eq!(patched.receipt.details.cells_created, 2);
    assert_eq!(patched.receipt.details.cells_updated, 2);
    // Present: the same type and payload with a new kind.
    let patched = db.patch(
        branch,
        RecordOp::patch(key(1), B).attribute("c", CellValue::from_u32(3)),
    );
    patched.result.unwrap();
    assert_eq!(patched.receipt.details.cells_updated, 1);
    // New cells, not a new record: nothing else changed.
    assert_eq!(untouched(), before);

    // The record holds the new values with the kinds the steps named.
    let record = db.read(ReadTarget::Branch(branch), key(1));
    let user = user_cells(&record);
    assert_eq!(
        user,
        [
            (b"a".as_slice(), &attribute(CellValue::from_u32(2))),
            (b"b", &CellValue::from_bytes(b"x")),
            (b"c", &attribute(CellValue::from_u32(3))),
            (b"d", &attribute(CellValue::from_u32(4))),
        ]
    );
}

/// "| missing | `attribute` / `field` | create |"
/// "| present, different | `attribute` / `field` | update |"
/// "| present | remove | delete |"
#[test]
fn successive_patches_compose() {
    let (db, branch) = setup();
    // `x` is created, updated, removed with `b`, then created again.
    for op in [
        RecordOp::patch(key(1), B).field("x", CellValue::from_u32(1)),
        RecordOp::patch(key(1), B).field("x", CellValue::from_u32(2)),
        RecordOp::patch(key(1), B).remove("x").remove("b"),
        RecordOp::patch(key(1), B).field("x", CellValue::from_u32(3)),
    ] {
        db.patch(branch, op).result.unwrap();
    }
    let record = db.read(ReadTarget::Branch(branch), key(1));
    let user = user_cells(&record);
    assert_eq!(
        user,
        [
            (b"a".as_slice(), &attribute(CellValue::from_u32(1))),
            (b"x", &CellValue::from_u32(3)),
        ]
    );
}

/// "| present, equal to the new value (type, kind and payload) | `attribute` / `field`
/// | **no-op**: nothing written or counted |"
#[test]
fn an_equal_write_is_a_no_op() {
    let (db, branch) = setup();
    let before = state(&db, branch);
    let patched = db.patch(
        branch,
        RecordOp::patch(key(1), B)
            .attribute("a", CellValue::from_u32(1))
            .field("b", CellValue::from_str("x")),
    );
    patched.result.unwrap();
    // Nothing counted, nothing written, index terms included: the branch seals to
    // the roots of an untouched one.
    assert_eq!(patched.receipt.details, Details::default());
    assert_eq!(state(&db, branch), before);
    let untouched = db.begin().unwrap();
    assert_eq!(db.sealed_roots(branch), db.sealed_roots(untouched));

    // Equal to a value this branch wrote, not a committed one: a no-op too. Only the
    // count can show it; the branch keeps no undo entry for a repeated value.
    let branch = db.begin().unwrap();
    let write = || RecordOp::patch(key(1), B).field("b", CellValue::from_str("y"));
    db.patch(branch, write()).result.unwrap();
    let patched = db.patch(branch, write());
    patched.result.unwrap();
    assert_eq!(patched.receipt.details, Details::default());
}

/// "| present, different | `attribute` / `field` | update |"
/// "| present | remove | delete |"
/// "| present | `set` (agreed) | the new value with the stored cell's kind; a no-op if
/// type and payload are also unchanged, an update otherwise |"
#[test]
fn changes_index_like_a_direct_create() {
    // A record committed, then patched and committed again: the head's record,
    // `#meta` included, and its index root. The state roots differ through `#roots`.
    let state = |create: RecordOp<op::Create>, patch: Option<RecordOp<op::Patch>>| {
        let db = TestDb::new();
        let branch = db.begin().unwrap();
        db.create(branch, create).result.unwrap();
        db.commit(branch).unwrap();
        let branch = db.begin().unwrap();
        if let Some(patch) = patch {
            db.patch(branch, patch).result.unwrap();
        }
        db.commit(branch).unwrap();
        let record = db.read(ReadTarget::Head, key(1));
        (record, db.sealed_roots(db.begin().unwrap()).1)
    };
    // An attribute updated, flipped to a field, removed, and `set` to another type;
    // a field `set`, and one flipped to an attribute.
    let patched = state(
        RecordOp::create(key(1), B)
            .attribute("a", CellValue::from_u32(1))
            .attribute("b", CellValue::from_str("x"))
            .attribute("c", CellValue::from_u32(5))
            .attribute("d", CellValue::from_u32(7))
            .field("e", CellValue::from_u32(9))
            .field("f", CellValue::from_str("y")),
        Some(
            RecordOp::patch(key(1), B)
                .attribute("a", CellValue::from_u32(2))
                .field("b", CellValue::from_str("x"))
                .remove("c")
                .set("d", CellValue::from_u64(8))
                .set("e", CellValue::from_u32(10))
                .attribute("f", CellValue::from_str("y")),
        ),
    );
    // The final record, created directly.
    let direct = state(
        RecordOp::create(key(1), B)
            .attribute("a", CellValue::from_u32(2))
            .field("b", CellValue::from_str("x"))
            .attribute("d", CellValue::from_u64(8))
            .field("e", CellValue::from_u32(10))
            .attribute("f", CellValue::from_str("y")),
        None,
    );
    assert_eq!(patched, direct);
}

/// "| present, different | `attribute` / `field` | update |"
/// "| present | remove | delete |"
#[test]
fn changes_share_index_terms_with_another_record() {
    // `key(2)` has `a = 1`, committed or created on the patching branch; `key(1)` has
    // `a`, committed, and is patched. The index root after the patch.
    let index_root = |a: u32, key2_committed: bool, patch: RecordOp<op::Patch>| {
        let db = TestDb::new();
        let key2 = || RecordOp::create(key(2), B).attribute("a", CellValue::from_u32(1));
        let branch = db.begin().unwrap();
        db.create(
            branch,
            RecordOp::create(key(1), B).attribute("a", CellValue::from_u32(a)),
        )
        .result
        .unwrap();
        if key2_committed {
            db.create(branch, key2()).result.unwrap();
        }
        db.commit(branch).unwrap();
        let branch = db.begin().unwrap();
        if !key2_committed {
            db.create(branch, key2()).result.unwrap();
        }
        db.patch(branch, patch).result.unwrap();
        db.sealed_roots(branch).1
    };
    let patch = || RecordOp::patch(key(1), B);
    for key2_committed in [true, false] {
        // Leaving `a = 1`, by an update or a remove, keeps the term `key(2)` holds:
        // the same as leaving `a = 9`.
        for change in [
            patch().attribute("a", CellValue::from_u32(2)),
            patch().remove("a"),
        ] {
            assert_eq!(
                index_root(1, key2_committed, change.clone()),
                index_root(9, key2_committed, change)
            );
        }
        // Joining `a = 1` from `a = 9` is the same as holding it from the start.
        assert_eq!(
            index_root(
                9,
                key2_committed,
                patch().attribute("a", CellValue::from_u32(1))
            ),
            index_root(1, key2_committed, patch())
        );
    }
}

/// "| present, different | `attribute` / `field` | update |"
/// "| missing | `attribute` / `field` | create |"
/// "| present | remove | delete |"
#[test]
fn changes_a_later_patch_reverts_seal_like_none() {
    let (db, branch) = setup();
    // The committed `a` updated and back, and `z` created and removed.
    for op in [
        RecordOp::patch(key(1), B)
            .attribute("a", CellValue::from_u32(2))
            .attribute("z", CellValue::from_u32(1)),
        RecordOp::patch(key(1), B)
            .attribute("a", CellValue::from_u32(1))
            .remove("z"),
    ] {
        db.patch(branch, op).result.unwrap();
    }
    // Index terms included: the committed record's term left and joined again.
    let untouched = db.begin().unwrap();
    assert_eq!(db.sealed_roots(branch), db.sealed_roots(untouched));
}

/// "| present | remove | delete |"
/// "| missing | remove | no-op |"
#[test]
fn remove_deletes_present_cells_and_skips_missing_ones() {
    let (db, branch) = setup();
    // A present cell is deleted.
    let removed = db.patch(branch, RecordOp::patch(key(1), B).remove("a"));
    removed.result.unwrap();
    assert_eq!(removed.receipt.details.cells_deleted, 1);
    let before = state(&db, branch);
    assert_eq!(before.1.cells.get("a".as_bytes()), None);

    // A missing one is a no-op: nothing counted or written.
    let removed = db.patch(branch, RecordOp::patch(key(1), B).remove("a").remove("z"));
    removed.result.unwrap();
    assert_eq!(removed.receipt.details, Details::default());
    assert_eq!(state(&db, branch), before);
}

/// "| present | `set` (agreed) | the new value with the stored cell's kind; a no-op if
/// type and payload are also unchanged, an update otherwise |"
#[test]
fn set_keeps_the_stored_kind() {
    let (db, branch) = setup();
    let record = || db.read(ReadTarget::Branch(branch), key(1));
    // The same type and payload, whatever kind the value carries: a no-op.
    let before = state(&db, branch);
    let set = db.patch(
        branch,
        RecordOp::patch(key(1), B)
            .set("a", CellValue::from_u32(1))
            .set("b", attribute(CellValue::from_str("x"))),
    );
    set.result.unwrap();
    assert_eq!(set.receipt.details, Details::default());
    assert_eq!(state(&db, branch), before);

    // A new payload or type: an update, keeping the stored kind.
    let set = db.patch(
        branch,
        RecordOp::patch(key(1), B)
            .set("a", CellValue::from_u64(2))
            .set("b", attribute(CellValue::from_str("y"))),
    );
    set.result.unwrap();
    assert_eq!(set.receipt.details.cells_updated, 2);
    assert_eq!(
        user_cells(&record()),
        [
            (b"a".as_slice(), &attribute(CellValue::from_u64(2))),
            (b"b", &CellValue::from_str("y")),
        ]
    );

    // The same payload under another type: an update, keeping the stored kind.
    let set = db.patch(
        branch,
        RecordOp::patch(key(1), B).set("a", CellValue::from_bytes8([0, 0, 0, 0, 0, 0, 0, 2])),
    );
    set.result.unwrap();
    assert_eq!(set.receipt.details.cells_updated, 1);
    assert_eq!(
        record().cells.get("a".as_bytes()),
        Some(&attribute(CellValue::from_bytes8([0, 0, 0, 0, 0, 0, 0, 2])))
    );

    // After this branch flips a kind, `set` keeps the flipped one.
    db.patch(
        branch,
        RecordOp::patch(key(1), B)
            .field("a", CellValue::from_u32(1))
            .attribute("b", CellValue::from_str("y")),
    )
    .result
    .unwrap();
    db.patch(
        branch,
        RecordOp::patch(key(1), B)
            .set("a", CellValue::from_u32(2))
            .set("b", CellValue::from_str("z")),
    )
    .result
    .unwrap();
    assert_eq!(
        user_cells(&record()),
        [
            (b"a".as_slice(), &CellValue::from_u32(2)),
            (b"b", &attribute(CellValue::from_str("z"))),
        ]
    );
}

/// "| missing | `set` (agreed) | **`CellNotFound { name }`**; the whole patch fails and
/// nothing is written |"
#[test]
fn a_recreated_record_has_none_of_the_old_cells_to_set() {
    // Deleted and re-created empty on the patching branch, or in earlier commits:
    // `a` and `b` belonged to the old record.
    for committed in [false, true] {
        let (db, mut branch) = setup();
        db.delete(branch, RecordOp::delete(key(1), B))
            .result
            .unwrap();
        if committed {
            db.commit(branch).unwrap();
            branch = db.begin().unwrap();
        }
        db.create(branch, RecordOp::create(key(1), B))
            .result
            .unwrap();
        if committed {
            db.commit(branch).unwrap();
            branch = db.begin().unwrap();
        }
        assert_err!(
            db.patch(
                branch,
                RecordOp::patch(key(1), B).set("a", CellValue::from_u32(2))
            )
            .result,
            ApiError::CellNotFound { name } if name.as_bytes() == b"a"
        );
        // So a write of an old name creates the cell, and removing one is a no-op.
        let written = db.patch(
            branch,
            RecordOp::patch(key(1), B).attribute("a", CellValue::from_u32(2)),
        );
        written.result.unwrap();
        assert_eq!(
            (
                written.receipt.details.cells_created,
                written.receipt.details.cells_updated
            ),
            (1, 0)
        );
        let removed = db.patch(branch, RecordOp::patch(key(1), B).remove("b"));
        removed.result.unwrap();
        assert_eq!(removed.receipt.details, Details::default());
    }
}

/// "| missing | `set` (agreed) | **`CellNotFound { name }`**; the whole patch fails and
/// nothing is written |"
/// "The whole patch is one atomic branch operation."
#[test]
fn a_failed_patch_writes_nothing() {
    let (db, branch) = setup();
    db.patch(branch, RecordOp::patch(key(1), B).remove("b"))
        .result
        .unwrap();
    let before = state(&db, branch);
    // Valid changes to `a` and `c`, then a `set` of `b`, removed on this branch, or
    // of `z`, never written: nothing is counted, though for `z` the changes to `a`
    // and `c` were planned first.
    for missing in ["b", "z"] {
        let failed = db.patch(
            branch,
            RecordOp::patch(key(1), B)
                .field("a", CellValue::from_u32(2))
                .field("c", CellValue::from_u32(3))
                .set(missing, CellValue::from_u32(1)),
        );
        assert_err!(
            failed.result,
            ApiError::CellNotFound { name } if name.as_bytes() == missing.as_bytes()
        );
        assert_eq!(failed.receipt.details, Details::default());
    }
    // Admission failures, with the same valid changes.
    for op in [
        RecordOp::patch(key(1), B)
            .field("a", CellValue::from_u32(2))
            .remove("x y"),
        RecordOp::patch(key(1), B)
            .field("a", CellValue::from_u32(2))
            .field("s", CellValue::from_str(&"a".repeat(65))),
        // Admission comes before the missing cell.
        RecordOp::patch(key(1), B)
            .set("z", CellValue::from_u32(1))
            .field("s", CellValue::from_str(&"a".repeat(65))),
    ] {
        let failed = db.patch(branch, op);
        assert_err!(failed.result, ApiError::InvalidArgument { .. });
        assert_eq!(failed.receipt.details, Details::default());
    }
    // None of them changed the record or the branch.
    assert_eq!(state(&db, branch), before);
}

/// "An empty patch is valid on an existing record."
#[test]
fn an_empty_patch_is_valid_on_an_existing_record() {
    let (db, branch) = setup();
    let before = state(&db, branch);
    let patched = db.patch(branch, RecordOp::patch(key(1), B));
    patched.result.unwrap();
    // A no-op.
    assert_eq!(patched.receipt.details, Details::default());
    assert_eq!(state(&db, branch), before);
}

/// "Removing the last user cell leaves an empty record."
#[test]
fn removing_the_last_user_cell_leaves_an_empty_record() {
    let (db, branch) = setup();
    db.patch(branch, RecordOp::patch(key(1), B).remove("a").remove("b"))
        .result
        .unwrap();
    // Only `#key` and `#meta` remain, and `#meta` counts nothing.
    let record = db.read(ReadTarget::Branch(branch), key(1));
    let names = cell_names(&record);
    assert_eq!(names, [b"#key".as_slice(), b"#meta"]);
    assert_eq!(record.meta(), Some(RecordMeta::default()));
    // Committed, the head reads the same empty record.
    db.commit(branch).unwrap();
    assert_eq!(db.read(ReadTarget::Head, key(1)), record);
    let branch = db.begin().unwrap();
    // The record is still there to patch.
    db.patch(
        branch,
        RecordOp::patch(key(1), B).field("a", CellValue::from_u32(1)),
    )
    .result
    .unwrap();
}

/// "`#meta` is rewritten only if its counts change."
#[test]
fn meta_is_rewritten_only_if_its_counts_change() {
    // The undo entries a patch adds on a fresh branch, and `#meta` before and after.
    // Assumes one entry per cell written, which the spec does not promise.
    let patched = |op| {
        let (db, branch) = setup();
        let meta = || db.read(ReadTarget::Branch(branch), key(1)).meta().unwrap();
        let (version_before, before) = (db.version(branch), meta());
        db.patch(branch, op).result.unwrap();
        (db.version(branch) - version_before, before, meta())
    };
    // An update of the same size keeps the counts: only the cell is written.
    let (same, before, after) =
        patched(RecordOp::patch(key(1), B).field("b", CellValue::from_str("y")));
    assert_eq!(after, before);
    // A new type of the same size keeps the counts.
    let (retyped, before, after) =
        patched(RecordOp::patch(key(1), B).attribute("a", CellValue::from_bytes4([0, 0, 0, 2])));
    assert_eq!(after, before);
    assert_eq!(retyped, same);
    // A longer value changes `cell_bytes`: `#meta` is written too.
    let (longer, before, after) =
        patched(RecordOp::patch(key(1), B).field("b", CellValue::from_str("yy")));
    assert_eq!(after.cell_bytes, before.cell_bytes + 1);
    assert_eq!(longer, same + 1);
    // The same value as an attribute changes the indexed counts: `#meta` is written.
    let (flipped, before, after) =
        patched(RecordOp::patch(key(1), B).attribute("b", CellValue::from_str("x")));
    assert_eq!(after.indexed_cells, before.indexed_cells + 1);
    // |name| + 2 + |value| for `b` = "x".
    assert_eq!(after.index_bytes, before.index_bytes + 4);
    assert_eq!(after.cell_bytes, before.cell_bytes);
    assert_eq!(flipped, same + 1);
    // A field removed and another of the same size created: the counts balance, so
    // only the two cells are written.
    let (balanced, before, after) = patched(
        RecordOp::patch(key(1), B)
            .remove("b")
            .field("c", CellValue::from_str("y")),
    );
    assert_eq!(after, before);
    assert_eq!(balanced, 2 * same);
}

/// "`#meta` is rewritten only if its counts change."
/// "The whole patch is one atomic branch operation."
#[test]
fn nested_rollbacks_keep_meta_with_its_cells() {
    let (db, _) = setup();
    let patch = || RecordOp::patch(key(1), B);
    // A patch that changes the counts and one that does not, in either order.
    let longer = patch().field("b", CellValue::from_str("yy"));
    let same = patch().attribute("a", CellValue::from_u32(2));
    for (outer, inner) in [
        (longer.clone(), same.clone()),
        (same, longer),
        // The same cell written in both frames, the inner one changing the counts.
        (
            patch().attribute("a", CellValue::from_u32(2)),
            patch().attribute("a", CellValue::from_u64(3)),
        ),
        // The inner frame removes a cell the outer one created.
        (
            patch().attribute("c", CellValue::from_u32(3)),
            patch().remove("c"),
        ),
    ] {
        let branch = db.begin().unwrap();
        db.checkpoint(branch).unwrap();
        db.patch(branch, outer.clone()).result.unwrap();
        db.checkpoint(branch).unwrap();
        db.patch(branch, inner).result.unwrap();
        db.rollback(branch).unwrap();
        // The outer patch alone, `#meta` included.
        let expected = db.begin().unwrap();
        db.patch(expected, outer).result.unwrap();
        assert_eq!(state(&db, branch), state(&db, expected));
        // Rolled back too, the outer patch leaves nothing, index terms included.
        db.rollback(branch).unwrap();
        let untouched = db.begin().unwrap();
        assert_eq!(db.sealed_roots(branch), db.sealed_roots(untouched));
    }
}
