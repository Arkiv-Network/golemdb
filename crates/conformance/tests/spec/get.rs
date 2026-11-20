//! Record operations: `get`.
//!
//! Where the spec and the code disagree, or the spec is silent, so nothing here
//! tests it:
//! - `.only` with a name over `#maxCellNameLen`: the spec says the call checks
//!   `#maxCellNameLen` without saying whether that includes `get`; the code returns
//!   the record without that cell.
//! - `.only` duplicates: the spec says they collapse; the read result collapses
//!   them, but `RecordOp::names()` returns them as given.
//! - `.only` given twice: the spec is silent; the code keeps the last list.
//!
//! Not reachable through the public API:
//! - a committed read where the binding and `#key` disagree (corrupt state), and a
//!   user record without `#meta` (`Internal`): both need corrupted state;
//! - `Head` reading the record in one store snapshot: needs concurrent commits.

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

/// "The builder checks the name grammar only, not `#maxCellNameLen` or the value
/// limits; the call checks those. `.only` takes names as raw bytes (`&str` or
/// `&[u8]`), so reserved names such as `#meta` can be projected; duplicates
/// collapse."
#[test]
fn projection_takes_raw_names_and_collapses_duplicates() {
    let db = setup();
    let record = db
        .get(
            ReadTarget::Head,
            RecordOp::get(key(1), B).only([b"a".as_slice(), b"a", b"#meta", b"x y"]),
        )
        .result
        .unwrap();
    // One `a` and `#meta`, in name order; `x y`, outside the write grammar, is
    // not an error, just missing.
    let names = cell_names(&record);
    assert_eq!(names, [b"#meta".as_slice(), b"a"]);
}

/// "`Branch(id)`: work in progress, including every successful uncommitted
/// mutation."
#[test]
fn branch_reads_see_uncommitted_writes_and_head_does_not() {
    let db = setup();
    let branch = db.begin().unwrap();
    let other = db.begin().unwrap();
    db.patch(
        branch,
        RecordOp::patch(key(1), B).field("b", CellValue::from_str("y")),
    )
    .result
    .unwrap();
    let b = |target| db.read(target, key(1)).cells.get("b".as_bytes()).cloned();
    // The branch sees the patch; the head and another branch over it still have
    // the committed value.
    assert_eq!(
        b(ReadTarget::Branch(branch)),
        Some(CellValue::from_str("y"))
    );
    assert_eq!(b(ReadTarget::Head), Some(CellValue::from_str("x")));
    assert_eq!(b(ReadTarget::Branch(other)), Some(CellValue::from_str("x")));

    // A rolled-back patch is no longer seen.
    db.checkpoint(branch).unwrap();
    db.patch(
        branch,
        RecordOp::patch(key(1), B).field("b", CellValue::from_str("z")),
    )
    .result
    .unwrap();
    db.rollback(branch).unwrap();
    assert_eq!(
        b(ReadTarget::Branch(branch)),
        Some(CellValue::from_str("y"))
    );

    // A projection sees the same: the patched `b`, and no `a` once removed.
    db.patch(branch, RecordOp::patch(key(1), B).remove("a"))
        .result
        .unwrap();
    let projected = db
        .get(
            ReadTarget::Branch(branch),
            RecordOp::get(key(1), B).only(["a", "b"]),
        )
        .result
        .unwrap();
    assert_eq!(
        cells(&projected),
        [(b"b".as_slice(), &CellValue::from_str("y"))]
    );

    // A create shows on the branch only.
    db.create(branch, RecordOp::create(key(2), B))
        .result
        .unwrap();
    assert_eq!(db.read(ReadTarget::Branch(branch), key(2)).key, key(2));
    for target in [ReadTarget::Head, ReadTarget::Branch(other)] {
        assert_err!(
            db.get(target, RecordOp::get(key(2), B)).result,
            ApiError::NotFound
        );
    }

    // So does a delete.
    let committed = db.read(ReadTarget::Head, key(1));
    db.delete(branch, RecordOp::delete(key(1), B))
        .result
        .unwrap();
    assert_err!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(key(1), B))
            .result,
        ApiError::NotFound
    );
    for target in [ReadTarget::Head, ReadTarget::Branch(other)] {
        assert_eq!(db.read(target, key(1)), committed);
    }
    // Discarded, the branch leaves the head as it was.
    db.discard(branch).unwrap();
    assert_eq!(db.read(ReadTarget::Head, key(1)), committed);
}

/// "A sealed branch stays readable and shows exactly the sealed state, except that
/// `#roots` does not show the new root until commit."
#[test]
fn sealed_branch_is_readable() {
    let db = setup();
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key(2), B).field("c", CellValue::from_u32(3)),
    )
    .result
    .unwrap();
    db.seal(branch).unwrap();
    // The sealed branch still reads its own write.
    assert_eq!(
        db.read(ReadTarget::Branch(branch), key(2))
            .cells
            .get("c".as_bytes()),
        Some(&CellValue::from_u32(3))
    );
    // `#roots` on the sealed branch is still the head's.
    let roots = |target| db.read(target, reserved_key("#roots")).cells;
    let head_roots = roots(ReadTarget::Head);
    assert_eq!(roots(ReadTarget::Branch(branch)), head_roots);
    // What it shows is exactly what commit makes the head: the record, the
    // allocator and the binding.
    let keys = [key(2), reserved_key("#alloc"), reserved_key("#recordKeys")];
    let sealed = keys.map(|k| db.read(ReadTarget::Branch(branch), k));
    db.commit(branch).unwrap();
    assert_eq!(keys.map(|k| db.read(ReadTarget::Head, k)), sealed);
    // Commit then adds the new root.
    assert_eq!(roots(ReadTarget::Head).len(), head_roots.len() + 1);
}

/// "`Commit(c)`: only the current head is supported."
///
/// TODO: review once history lands; past commits then become readable.
#[test]
fn head_and_commit_of_head_read_the_same_record() {
    let db = setup();
    let head = db.head().unwrap();
    assert_eq!(
        db.read(ReadTarget::Head, key(1)),
        db.read(ReadTarget::Commit(head), key(1))
    );
}

/// "`Commit(c)`: only the current head is supported."
///
/// TODO: review once history lands; past commits then become readable.
#[test]
fn genesis_is_readable_while_it_is_the_head() {
    let db = TestDb::new();
    assert_eq!(db.head().unwrap(), CommitId::GENESIS);
    let alloc = reserved_key("#alloc");
    assert_eq!(
        db.read(ReadTarget::Commit(CommitId::GENESIS), alloc),
        db.read(ReadTarget::Head, alloc)
    );
}

/// "Any other commit is `CommitUnavailable { requested, head }`."
///
/// TODO: review once history lands; past commits then become readable.
#[test]
fn other_commits_are_unavailable() {
    let db = setup();
    // An empty commit still moves the head.
    db.commit(db.begin().unwrap()).unwrap();
    let head = db.head().unwrap();
    assert_eq!(head, CommitId::new(2));
    // Genesis, an earlier commit, the next one and one far ahead.
    for requested in [
        CommitId::GENESIS,
        CommitId::new(1),
        CommitId::new(3),
        CommitId::new(u64::MAX),
    ] {
        assert_err!(
            db.get(ReadTarget::Commit(requested), RecordOp::get(key(1), B)).result,
            ApiError::CommitUnavailable { requested: r, head: h } if r == requested && h == head
        );
    }
}

/// "Order of checks: the target first, at no cost (an invalid branch handle →
/// `HandleInvalid`; an unavailable commit → `CommitUnavailable`); then the binding
/// (`NotFound`); then the cells."
#[test]
fn target_errors_come_before_not_found() {
    let db = setup();
    let stale = db.begin().unwrap();
    let committed = db.begin().unwrap();
    db.commit(committed).unwrap();
    let discarded = db.begin().unwrap();
    db.discard(discarded).unwrap();
    // A missing key on a stale, committed, discarded or unknown branch.
    for branch in [stale, committed, discarded, BranchId::new(u64::MAX)] {
        assert_err!(
            db.get(ReadTarget::Branch(branch), RecordOp::get(key(9), B))
                .result,
            ApiError::HandleInvalid
        );
    }
    // A missing key on a commit that does not exist yet.
    assert_err!(
        db.get(
            ReadTarget::Commit(CommitId::new(u64::MAX)),
            RecordOp::get(key(9), B)
        )
        .result,
        ApiError::CommitUnavailable { .. }
    );
}

/// "A full read returns every cell, `#key` and `#meta` included."
#[test]
fn full_read_includes_key_and_meta() {
    let db = setup();
    // Neighbours: `key(2)` and `key(3)` take the next two record IDs.
    let branch = db.begin().unwrap();
    for (n, cell) in [(2, "c"), (3, "d")] {
        db.create(
            branch,
            RecordOp::create(key(n), B).field(cell, CellValue::from_u32(n.into())),
        )
        .result
        .unwrap();
    }
    db.commit(branch).unwrap();
    // The record between them has only its own cells.
    let neighbour = db.read(ReadTarget::Head, key(2));
    let names = cell_names(&neighbour);
    assert_eq!(names, [b"#key".as_slice(), b"#meta", b"c"]);

    let record = db.read(ReadTarget::Head, key(1));
    assert_eq!(record.key, key(1));
    // `#meta` holds `cells`, `cell_bytes`, `indexed_cells` and `index_bytes`, each
    // `u64` BE. `a` is (8 + 1) + (1 + 4), indexed as 1 + 2 + 4; `b` is
    // (8 + 1) + (1 + 1).
    let meta: Vec<u8> = [2u64, 14 + 11, 1, 7]
        .iter()
        .flat_map(|count| count.to_be_bytes())
        .collect();
    assert_eq!(
        cells(&record),
        [
            (b"#key".as_slice(), &CellValue::from_bytes32(key(1).0)),
            (b"#meta", &CellValue::from_bytes32(meta.try_into().unwrap())),
            (b"a", &attribute(CellValue::from_u32(1))),
            (b"b", &CellValue::from_str("x")),
        ]
    );
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
    let names = cell_names(&record);
    assert_eq!(names, [b"#key".as_slice(), b"b"]);
    assert_eq!(record.key, key(1));

    // An empty projection, on every target: no cells for an existing record,
    // `NotFound` for a missing one.
    let no_cells = |k| RecordOp::get(k, B).only(std::iter::empty::<&str>());
    let branch = db.begin().unwrap();
    let head = db.head().unwrap();
    for target in [
        ReadTarget::Head,
        ReadTarget::Branch(branch),
        ReadTarget::Commit(head),
    ] {
        let record = db.get(target, no_cells(key(1))).result.unwrap();
        assert_eq!(record.key, key(1));
        assert!(record.cells.is_empty());
        assert_err!(db.get(target, no_cells(key(2))).result, ApiError::NotFound);
    }
}

/// "A missing record is `NotFound`."
#[test]
fn missing_record_is_not_found_on_every_target() {
    let db = setup();
    // `key(1)` is deleted and the delete committed; `key(9)` never existed.
    let delete = db.begin().unwrap();
    db.delete(delete, RecordOp::delete(key(1), B))
        .result
        .unwrap();
    db.commit(delete).unwrap();
    let branch = db.begin().unwrap();
    let head = db.head().unwrap();
    for target in [
        ReadTarget::Head,
        ReadTarget::Branch(branch),
        ReadTarget::Commit(head),
    ] {
        // A reserved-looking key outside the catalogue is not bound either.
        for k in [key(1), key(9), reserved_key("#nope")] {
            // Full read and projection alike.
            assert_err!(
                db.get(target, RecordOp::get(k, B)).result,
                ApiError::NotFound
            );
            assert_err!(
                db.get(target, RecordOp::get(k, B).only(["a"])).result,
                ApiError::NotFound
            );
        }
    }
}
