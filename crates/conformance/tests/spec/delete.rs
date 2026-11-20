//! Record operations: `delete`.
//!
//! Agreed changes the code already implements, tested as agreed:
//! - the handle before the reserved key: the spec says the implementations check
//!   reserved keys before the handle; the code checks the handle first.
//!
//! Where the spec is stale or silent, so nothing here tests it:
//! - A failed delete writing nothing: the spec says only that it counts nothing
//!   (Receipts), not that it writes nothing, as it does for `create` and `patch`.
//!   Only `Reserved` is checked, as the reserved record must survive.
//! - Reserved-looking keys: whether a zero-padded `#…` key outside the catalogue,
//!   or the all-zero key, is reserved is open; the code treats both as user keys,
//!   so a delete of one that is not bound is `NotFound`.
//!
//! Not reachable through the public API:
//! - a user record without `#meta` (`Internal`), or whose `#meta` does not match its
//!   live cells (corrupt state): both need corrupted state.

use golemdb_conformance::*;

/// `key(1)` with `a`, a `u32` attribute 1, and `b`, a `str` field "x", and `key(2)`
/// with `c`, both committed.
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
    db.create(
        branch,
        RecordOp::create(key(2), B).field("c", CellValue::from_u32(2)),
    )
    .result
    .unwrap();
    db.commit(branch).unwrap();
    db
}

/// "Order of checks: handle → `HandleInvalid` / `Sealed`; reserved key → `Reserved`;
/// key not bound → `NotFound`."
#[test]
fn handle_errors_come_first() {
    let db = setup();
    // A delete that admission would reject, one that record state would, and a
    // valid one.
    let ops = || {
        [
            RecordOp::delete(reserved_key("#alloc"), B),
            RecordOp::delete(key(9), B),
            RecordOp::delete(key(1), B),
        ]
    };
    // On a stale branch: one per op, as the first call that detects one removes it.
    for op in ops() {
        let stale = db.begin().unwrap();
        db.commit(db.begin().unwrap()).unwrap();
        assert_err!(db.delete(stale, op).result, ApiError::HandleInvalid);
    }
    // A sealed branch gone stale is `HandleInvalid` too, not `Sealed`.
    for op in ops() {
        let stale = db.begin().unwrap();
        db.seal(stale).unwrap();
        db.commit(db.begin().unwrap()).unwrap();
        assert_err!(db.delete(stale, op).result, ApiError::HandleInvalid);
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
            assert_err!(db.delete(branch, op).result, ApiError::HandleInvalid);
        }
    }
    // On a sealed branch.
    for op in ops() {
        assert_err!(db.delete(sealed, op).result, ApiError::Sealed);
    }
}

/// "Order of checks: handle → `HandleInvalid` / `Sealed`; reserved key → `Reserved`;
/// key not bound → `NotFound`."
#[test]
fn reserved_keys_are_reserved() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    for name in RESERVED_NAMES {
        let before = db.read(ReadTarget::Branch(branch), reserved_key(name));
        assert_err!(
            db.delete(branch, RecordOp::delete(reserved_key(name), B))
                .result,
            ApiError::Reserved
        );
        // The record and all its cells are still there.
        assert_eq!(
            db.read(ReadTarget::Branch(branch), reserved_key(name)),
            before
        );
    }
}

/// "Order of checks: handle → `HandleInvalid` / `Sealed`; reserved key → `Reserved`;
/// key not bound → `NotFound`."
#[test]
fn missing_record_is_not_found() {
    let db = setup();
    let branch = db.begin().unwrap();
    // `key(1)` deleted on this branch, `key(2)` deleted and committed, and `key(9)`
    // never bound.
    db.delete(branch, RecordOp::delete(key(2), B))
        .result
        .unwrap();
    db.commit(branch).unwrap();
    let branch = db.begin().unwrap();
    db.delete(branch, RecordOp::delete(key(1), B))
        .result
        .unwrap();
    for k in [key(1), key(2), key(9)] {
        assert_err!(
            db.delete(branch, RecordOp::delete(k, B)).result,
            ApiError::NotFound
        );
    }
}

/// "It then removes every live cell of the record, `#key` and `#meta` included, and
/// the binding."
#[test]
fn delete_removes_every_cell_and_the_binding() {
    let db = setup();
    let branch = db.begin().unwrap();
    // A cell written on this branch is live too.
    db.patch(
        branch,
        RecordOp::patch(key(1), B).field("d", CellValue::from_u32(4)),
    )
    .result
    .unwrap();
    let neighbour = db.read(ReadTarget::Branch(branch), key(2));
    let deleted = db.delete(branch, RecordOp::delete(key(1), B));
    deleted.result.unwrap();
    // The three user cells.
    assert_eq!(deleted.receipt.details.cells_deleted, 3);

    // No read finds the record, full or by any cell name, nor its binding.
    for op in [
        RecordOp::get(key(1), B),
        RecordOp::get(key(1), B).only(["#key", "#meta", "a", "b", "d"]),
    ] {
        assert_err!(
            db.get(ReadTarget::Branch(branch), op).result,
            ApiError::NotFound
        );
    }
    assert_eq!(db.record_id(branch, key(1)), None);
    // The neighbour is untouched.
    assert_eq!(db.read(ReadTarget::Branch(branch), key(2)), neighbour);

    // Committed, the head has neither the record nor its binding.
    db.commit(branch).unwrap();
    assert_err!(
        db.get(ReadTarget::Head, RecordOp::get(key(1), B)).result,
        ApiError::NotFound
    );
    assert_eq!(db.record_id(db.begin().unwrap(), key(1)), None);
    assert_eq!(db.read(ReadTarget::Head, key(2)), neighbour);
}

/// "It then removes every live cell of the record, `#key` and `#meta` included, and
/// the binding."
#[test]
fn delete_leaves_both_neighbours() {
    // `key(1)`, `key(2)` and `key(3)` committed as IDs 64, 65 and 66, the middle one
    // full or empty; then `key(2)` deleted.
    let deleted = |full: bool| {
        let db = TestDb::new();
        let branch = db.begin().unwrap();
        db.create(
            branch,
            RecordOp::create(key(1), B).attribute("a", CellValue::from_u32(1)),
        )
        .result
        .unwrap();
        let mut middle = RecordOp::create(key(2), B);
        if full {
            // Up to both per-record caps.
            for n in 0..RECORD_LIMITS.max_cells {
                let name = format!("c{n:02}");
                middle = if n < RECORD_LIMITS.max_indexed_cells {
                    middle.attribute(&name, CellValue::from_u64(n))
                } else {
                    middle.field(&name, CellValue::from_u64(n))
                };
            }
        }
        db.create(branch, middle).result.unwrap();
        db.create(
            branch,
            RecordOp::create(key(3), B).attribute("a", CellValue::from_u32(3)),
        )
        .result
        .unwrap();
        db.commit(branch).unwrap();
        let branch = db.begin().unwrap();
        let neighbours = || {
            (
                db.read(ReadTarget::Branch(branch), key(1)),
                db.read(ReadTarget::Branch(branch), key(3)),
            )
        };
        let before = neighbours();
        db.delete(branch, RecordOp::delete(key(2), B))
            .result
            .unwrap();
        // Both neighbours read as before, and the middle record and its binding are
        // gone, empty or not.
        assert_eq!(neighbours(), before);
        assert_eq!(db.record_id(branch, key(2)), None);
        assert_err!(
            db.get(ReadTarget::Branch(branch), RecordOp::get(key(2), B))
                .result,
            ApiError::NotFound
        );
        (before, db.sealed_roots(branch).1)
    };
    // Index terms included: the full record leaves the same index as the empty one.
    assert_eq!(deleted(true), deleted(false));
}

/// "It then removes every live cell of the record, `#key` and `#meta` included, and
/// the binding."
#[test]
fn delete_leaves_nothing_behind() {
    // Records created, optionally patched, and deleted on one branch: each takes
    // ID 64 and leaves the allocator at 65.
    let sealed = |create: RecordOp<op::Create>, patch: Option<RecordOp<op::Patch>>| {
        let db = TestDb::new();
        let branch = db.begin().unwrap();
        let k = create.record_key();
        db.create(branch, create).result.unwrap();
        if let Some(patch) = patch {
            db.patch(branch, patch).result.unwrap();
        }
        db.delete(branch, RecordOp::delete(k, B)).result.unwrap();
        // Neither the record nor its binding is left, so a second delete is
        // `NotFound`.
        assert_err!(
            db.get(ReadTarget::Branch(branch), RecordOp::get(k, B))
                .result,
            ApiError::NotFound
        );
        assert_eq!(db.record_id(branch, k), None);
        assert_err!(
            db.delete(branch, RecordOp::delete(k, B)).result,
            ApiError::NotFound
        );
        db.sealed_roots(branch)
    };
    let full = || {
        RecordOp::create(key(1), B)
            .attribute("a", CellValue::from_u32(1))
            .field("b", CellValue::from_str("x"))
    };
    let empty = sealed(RecordOp::create(key(1), B), None);
    // A zeroed `#meta` left behind would look the same on every side, so this does
    // not show that `#meta` is removed; no public read can tell.
    // No cell or index term is left under the old ID: the roots match an empty
    // record's, also when a patch added, updated and removed cells first.
    assert_eq!(sealed(full(), None), empty);
    assert_eq!(
        sealed(
            full(),
            Some(
                RecordOp::patch(key(1), B)
                    .attribute("a", CellValue::from_u32(2))
                    .remove("b")
                    .attribute("c", CellValue::from_str("y"))
            )
        ),
        empty
    );
    // Nor is `#key` or the binding, which would differ by key: another key gives
    // the same roots.
    assert_eq!(sealed(RecordOp::create(key(2), B), None), empty);
}

/// "It then removes every live cell of the record, `#key` and `#meta` included, and
/// the binding."
#[test]
fn delete_recreate_delete_on_one_branch_leaves_nothing() {
    // A committed `key(1)` at ID 64; on one branch, the deletes and a create in
    // between that takes ID 65.
    let sealed = |second: RecordKey| {
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
        db.delete(branch, RecordOp::delete(key(1), B))
            .result
            .unwrap();
        db.create(
            branch,
            RecordOp::create(second, B).attribute("a", CellValue::from_u32(2)),
        )
        .result
        .unwrap();
        db.delete(branch, RecordOp::delete(second, B))
            .result
            .unwrap();
        db.sealed_roots(branch)
    };
    // Re-creating `key(1)` seals like creating another key in its place.
    assert_eq!(sealed(key(1)), sealed(key(2)));
}

/// "It then removes every live cell of the record, `#key` and `#meta` included, and
/// the binding."
#[test]
fn delete_removes_what_the_branch_left_live() {
    // `key(1)` committed with `a` to `e`, attributes and fields alternating.
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    let mut create = RecordOp::create(key(1), B);
    for (n, name) in (0..).zip(["a", "b", "c", "d", "e"]) {
        create = if n % 2 == 0 {
            create.attribute(name, CellValue::from_u32(n))
        } else {
            create.field(name, CellValue::from_u32(n))
        };
    }
    db.create(branch, create).result.unwrap();
    db.commit(branch).unwrap();
    let patch = || RecordOp::patch(key(1), B);
    let new = || CellValue::from_u32(9);
    let all = vec!["a", "b", "c", "d", "e"];
    // Patches before the delete, and the user cells they leave live.
    for (patches, live) in [
        // `a` updated, `b` removed, and `aa` added between them.
        (
            vec![
                patch()
                    .attribute("a", new())
                    .remove("b")
                    .attribute("aa", new()),
            ],
            vec!["a", "aa", "c", "d", "e"],
        ),
        // `a` turned into a field: its committed index term must still go.
        (
            vec![patch().field("a", CellValue::from_u32(0))],
            all.clone(),
        ),
        // A removed committed cell is hidden from record scans; written back, it
        // shows again: alone, at the start, middle or end of a removed run, as the
        // last cell, or after every cell was removed.
        (
            vec![patch().remove("b"), patch().field("b", new())],
            all.clone(),
        ),
        (
            vec![
                patch().remove("b").remove("c").remove("d"),
                patch().field("b", new()),
            ],
            vec!["a", "b", "e"],
        ),
        (
            vec![
                patch().remove("b").remove("c").remove("d"),
                patch().field("c", new()),
            ],
            vec!["a", "c", "e"],
        ),
        (
            vec![
                patch().remove("b").remove("c").remove("d"),
                patch().field("d", new()),
            ],
            vec!["a", "d", "e"],
        ),
        (
            vec![patch().remove("e"), patch().field("e", new())],
            all.clone(),
        ),
        // Every cell removed.
        (
            vec![
                patch()
                    .remove("a")
                    .remove("b")
                    .remove("c")
                    .remove("d")
                    .remove("e"),
            ],
            vec![],
        ),
        (
            vec![
                patch()
                    .remove("a")
                    .remove("b")
                    .remove("c")
                    .remove("d")
                    .remove("e"),
                patch().field("c", new()),
            ],
            vec!["c"],
        ),
    ] {
        let patched = db.begin().unwrap();
        for op in patches {
            db.patch(patched, op).result.unwrap();
        }
        // A full read sees exactly the live cells, and the delete removes them.
        let record = db.read(ReadTarget::Branch(patched), key(1));
        let names: Vec<_> = user_cells(&record)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            live.iter().map(|name| name.as_bytes()).collect::<Vec<_>>()
        );
        let deleted = db.delete(patched, RecordOp::delete(key(1), B));
        deleted.result.unwrap();
        assert_eq!(deleted.receipt.details.cells_deleted, live.len() as u64);
        // The same as deleting the untouched record.
        let untouched = db.begin().unwrap();
        db.delete(untouched, RecordOp::delete(key(1), B))
            .result
            .unwrap();
        assert_eq!(db.sealed_roots(patched), db.sealed_roots(untouched));
    }
}

/// "It then removes every live cell of the record, `#key` and `#meta` included, and
/// the binding."
#[test]
fn delete_keeps_an_index_term_another_record_shares() {
    // `key(2)` has `a = 1`; `key(1)` has it too, or `a = 9`. Each is committed or
    // created on the deleting branch. Deleting `key(1)` leaves the same index
    // either way.
    let index_root = |a: u32, key1_committed: bool, key2_committed: bool| {
        let db = TestDb::new();
        let key1 = || RecordOp::create(key(1), B).attribute("a", CellValue::from_u32(a));
        let key2 = || RecordOp::create(key(2), B).attribute("a", CellValue::from_u32(1));
        let committed = db.begin().unwrap();
        let branch = if key1_committed || key2_committed {
            if key1_committed {
                db.create(committed, key1()).result.unwrap();
            }
            if key2_committed {
                db.create(committed, key2()).result.unwrap();
            }
            db.commit(committed).unwrap();
            db.begin().unwrap()
        } else {
            committed
        };
        if !key1_committed {
            db.create(branch, key1()).result.unwrap();
        }
        if !key2_committed {
            db.create(branch, key2()).result.unwrap();
        }
        db.delete(branch, RecordOp::delete(key(1), B))
            .result
            .unwrap();
        db.sealed_roots(branch).1
    };
    for key1_committed in [true, false] {
        for key2_committed in [true, false] {
            assert_eq!(
                index_root(1, key1_committed, key2_committed),
                index_root(9, key1_committed, key2_committed)
            );
        }
    }
}

/// "It then removes every live cell of the record, `#key` and `#meta` included, and
/// the binding."
#[test]
fn a_rolled_back_delete_restores_the_record() {
    // `key(1)` committed or created on the branch; then, in a rolled-back frame or
    // not at all, a delete, alone or with a re-create that takes the next ID. Then
    // a delete. The branch's record, binding, allocator and `version` before that
    // last delete, its details, and the roots after it.
    let run = |committed: bool, recreate: bool, frame: bool| {
        let db = TestDb::new();
        let mut branch = db.begin().unwrap();
        db.create(
            branch,
            RecordOp::create(key(1), B)
                .attribute("a", CellValue::from_u32(1))
                .field("b", CellValue::from_str("x")),
        )
        .result
        .unwrap();
        if committed {
            db.commit(branch).unwrap();
            branch = db.begin().unwrap();
        }
        if frame {
            db.checkpoint(branch).unwrap();
            db.delete(branch, RecordOp::delete(key(1), B))
                .result
                .unwrap();
            if recreate {
                db.create(
                    branch,
                    RecordOp::create(key(1), B).attribute("a", CellValue::from_u32(2)),
                )
                .result
                .unwrap();
                assert_eq!(db.record_id(branch, key(1)), Some(65));
            }
            db.rollback(branch).unwrap();
        }
        let state = (
            db.version(branch),
            db.read(ReadTarget::Branch(branch), key(1)),
            db.record_id(branch, key(1)),
            db.next_record_id(branch),
        );
        let deleted = db.delete(branch, RecordOp::delete(key(1), B));
        deleted.result.unwrap();
        (state, deleted.receipt.details, db.sealed_roots(branch))
    };
    // The rolled-back frame leaves the branch as it was: ID 64 with its cells, and
    // the allocator at 65. Its restored cells delete as if never touched.
    for committed in [true, false] {
        for recreate in [false, true] {
            assert_eq!(
                run(committed, recreate, true),
                run(committed, recreate, false)
            );
        }
    }
}

/// "It then removes every live cell of the record, `#key` and `#meta` included, and
/// the binding."
/// "The allocator does not move back; re-creating the key gives a new ID."
#[test]
fn a_recreated_record_has_only_its_new_cells() {
    // `key(1)` committed with the attribute `a = old` and the field `b`; deleted and
    // re-created with `a = 1` and `c` as ID 65, committed; then deleted again. The
    // head's record and index root after the re-create, and the index root after
    // the second delete. The state roots differ through `#roots`.
    let committed = |old: u32| {
        let db = TestDb::new();
        let branch = db.begin().unwrap();
        db.create(
            branch,
            RecordOp::create(key(1), B)
                .attribute("a", CellValue::from_u32(old))
                .field("b", CellValue::from_str("x")),
        )
        .result
        .unwrap();
        db.commit(branch).unwrap();
        let branch = db.begin().unwrap();
        db.delete(branch, RecordOp::delete(key(1), B))
            .result
            .unwrap();
        db.create(
            branch,
            RecordOp::create(key(1), B)
                .attribute("a", CellValue::from_u32(1))
                .field("c", CellValue::from_u32(3)),
        )
        .result
        .unwrap();
        assert_eq!(db.record_id(branch, key(1)), Some(65));
        db.commit(branch).unwrap();
        let record = db.read(ReadTarget::Head, key(1));
        let branch = db.begin().unwrap();
        let recreated = db.sealed_roots(branch).1;
        let branch = db.begin().unwrap();
        let deleted = db.delete(branch, RecordOp::delete(key(1), B));
        deleted.result.unwrap();
        // Only the current record's cells, `a` and `c`.
        assert_eq!(deleted.receipt.details.cells_deleted, 2);
        (record, recreated, db.sealed_roots(branch).1)
    };
    // Only the new cells, `b` gone and `a` the new one.
    let (record, recreated, deleted) = committed(1);
    assert_eq!(
        user_cells(&record),
        [
            (b"a".as_slice(), &attribute(CellValue::from_u32(1))),
            (b"c", &CellValue::from_u32(3)),
        ]
    );
    // No term of the old `a` is left under ID 64, before or after the second delete:
    // the same value or another, the index matches.
    assert_eq!(committed(9), (record, recreated, deleted));
}
