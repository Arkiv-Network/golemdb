//! Record operations: `RecordOp`.
//!
//! Agreed changes the code already implements, tested as agreed:
//! - `.set(n, v)`, a step apart from a write of the same value.
//!
//! Where the spec is stale or silent, so nothing here tests it:
//! - The create's key: the spec has `RecordOp::create(budget).key(k)` and a `.key`
//!   given twice error; the code takes the key in `RecordOp::create(key, budget)`.
//!   So `record_key()` returns a `RecordKey`, not an `Option`, on all four
//!   operations.
//! - The `op` markers: the spec's are empty enums; the code's are structs holding
//!   the steps. Callers cannot tell the difference.
//! - `validate()`, `value(name)`, `removes(name)`, `cells()`, `changes()` and
//!   `Change`: not in the code, which documents "no validation shortcut". It has
//!   `entries()` on create and patch instead: the steps in caller order,
//!   duplicates and invalid steps kept, the requested kind apart from the value's.
//! - `max_cost()`: in the code, not the spec.
//! - `.only` duplicates: the spec says they collapse; the read collapses them, as
//!   `get` tests, but `names()` returns them as given and equality counts them, so
//!   `only(["a", "a"])` is unequal to `only(["a"])`.
//! - `.only` given twice: the spec is silent; the code keeps the last list.
//! - Order in equality: the spec has no equality rule; its `cells()` and
//!   `changes()` list steps in name order, but the code compares steps, and
//!   projection names, in caller order, so the same ones given in another order
//!   are unequal.
//! - The value's own kind in equality: in the spec the builder sets the kind, so
//!   `.field("a", attribute(v))` and `.field("a", v)` write the same cell; the code
//!   keeps the value as given, kind included, and compares it, so they are unequal.
//!   Likewise for `.set`, which ignores the value's kind.
//!
//! Tested elsewhere:
//! - Builder errors, their messages and causes, the first one kept, and nothing
//!   written: `create` and `patch`.
//! - The name grammar checked by the builder, the limits by the call: `create`,
//!   `patch` and `data_model`.
//! - `.only` taking raw names, so `#meta` can be projected: `get`.
//!
//! Not covered here:
//! - Retrying after losing a commit race: whether one commit leaves another branch
//!   uncommittable is the Branches section's. That a clone runs like the original
//!   is tested here.
//! - The compile-time rules and `#[must_use]`: passing code does not show that
//!   forbidden calls fail to compile. The api crate's `compile_fail` doc tests
//!   check three rules: `.remove` on a create, a write on a get, and an operation
//!   passed to the wrong call. Nothing checks `.only` outside a get, `.set` on a
//!   create, or a write on a delete.

use golemdb_conformance::*;

/// "pub struct RecordOp<Op> { … }             // Clone + PartialEq + Eq + Debug,
/// #[must_use]"
/// "| `RecordOp::patch(k, budget)` | `.attribute(n, v)`, `.field(n, v)`, `.remove(n)`;
/// agreed: `.set(n, v)` | `set` keeps the stored kind |"
/// "| `names() -> Option<&[CellName]>` | get | the projection: `None` for the full
/// record, `Some([])` for an existence check |"
#[test]
fn record_ops_compare_by_key_budget_and_steps() {
    // The derives, required by the compiler.
    fn assert_traits<T: Clone + Eq + std::fmt::Debug>() {}
    assert_traits::<RecordOp<op::Create>>();
    assert_traits::<RecordOp<op::Patch>>();
    assert_traits::<RecordOp<op::Get>>();
    assert_traits::<RecordOp<op::Delete>>();
    let u32_1 = || CellValue::from_u32(1);
    let create = || RecordOp::create(key(1), B).field("a", u32_1());
    let patch = || RecordOp::patch(key(1), B).remove("a");
    let set = || RecordOp::patch(key(1), B).set("a", u32_1());
    let get = || RecordOp::get(key(1), B).only(["a"]);
    let delete = || RecordOp::delete(key(1), B);
    // A clone is equal.
    assert_eq!(create().clone(), create());
    assert_eq!(patch().clone(), patch());
    assert_eq!(set().clone(), set());
    assert_eq!(get().clone(), get());
    assert_eq!(delete().clone(), delete());
    // Another key or budget is not, on every operation.
    let other = Budget::Limited(1);
    assert_ne!(RecordOp::create(key(2), B).field("a", u32_1()), create());
    assert_ne!(
        RecordOp::create(key(1), other).field("a", u32_1()),
        create()
    );
    assert_ne!(RecordOp::patch(key(2), B).remove("a"), patch());
    assert_ne!(RecordOp::patch(key(1), other).remove("a"), patch());
    assert_ne!(RecordOp::get(key(2), B).only(["a"]), get());
    assert_ne!(RecordOp::get(key(1), other).only(["a"]), get());
    assert_ne!(RecordOp::delete(key(2), B), delete());
    assert_ne!(RecordOp::delete(key(1), other), delete());
    assert_ne!(
        RecordOp::delete(key(1), Budget::Limited(2)),
        RecordOp::delete(key(1), other)
    );
    // Nor another name, value, kind or step, or another number of steps.
    let create_ops = [
        RecordOp::create(key(1), B).field("b", u32_1()),
        RecordOp::create(key(1), B).field("a", CellValue::from_u32(2)),
        RecordOp::create(key(1), B).attribute("a", u32_1()),
        RecordOp::create(key(1), B),
        create().field("b", u32_1()),
    ];
    for op in create_ops {
        assert_ne!(op, create());
    }
    let patch_ops = [
        RecordOp::patch(key(1), B).remove("b"),
        RecordOp::patch(key(1), B).field("a", u32_1()),
        RecordOp::patch(key(1), B),
        patch().remove("b"),
    ];
    for op in patch_ops {
        assert_ne!(op, patch());
    }
    // A patch write of another kind or value; a `set` of another name or value.
    let write = || RecordOp::patch(key(1), B).field("a", u32_1());
    assert_ne!(RecordOp::patch(key(1), B).attribute("a", u32_1()), write());
    assert_ne!(
        RecordOp::patch(key(1), B).field("a", CellValue::from_u32(2)),
        write()
    );
    assert_ne!(RecordOp::patch(key(1), B).set("b", u32_1()), set());
    assert_ne!(
        RecordOp::patch(key(1), B).set("a", CellValue::from_u32(2)),
        set()
    );
    // A `set` is not a write of the same value, of either kind.
    assert_ne!(write(), set());
    assert_ne!(RecordOp::patch(key(1), B).remove("a"), set());
    assert_ne!(RecordOp::patch(key(1), B).attribute("a", u32_1()), set());
    // Another projection; a full read is not an existence check.
    assert_ne!(RecordOp::get(key(1), B).only(["b"]), get());
    assert_ne!(RecordOp::get(key(1), B), get());
    assert_ne!(
        RecordOp::get(key(1), B),
        RecordOp::get(key(1), B).only(std::iter::empty::<&str>())
    );
}

/// "An operation is also a plain value: it can be cloned to retry after losing a
/// commit race and compared in a mock."
/// "**Builder steps never fail.** The first problem is kept and reported by the call"
#[test]
fn a_clone_runs_like_the_original() {
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
    // Each write's clone on one branch and the original on another leave the same
    // state.
    let create = RecordOp::create(key(2), B)
        .attribute("a", CellValue::from_u32(1))
        .field("b", CellValue::from_str("x"));
    let (first, second) = (db.begin().unwrap(), db.begin().unwrap());
    db.create(first, create.clone()).result.unwrap();
    db.create(second, create).result.unwrap();
    assert_eq!(
        db.read(ReadTarget::Branch(first), key(2)),
        db.read(ReadTarget::Branch(second), key(2))
    );
    let patch = RecordOp::patch(key(1), B)
        .set("a", CellValue::from_u32(2))
        .remove("b");
    let (first, second) = (db.begin().unwrap(), db.begin().unwrap());
    db.patch(first, patch.clone()).result.unwrap();
    db.patch(second, patch).result.unwrap();
    assert_eq!(
        db.read(ReadTarget::Branch(first), key(1)),
        db.read(ReadTarget::Branch(second), key(1))
    );
    let delete = RecordOp::delete(key(1), B);
    let (first, second) = (db.begin().unwrap(), db.begin().unwrap());
    db.delete(first, delete.clone()).result.unwrap();
    db.delete(second, delete).result.unwrap();
    assert_eq!(db.record_id(first, key(1)), None);
    assert_eq!(db.record_id(second, key(1)), None);
    // A read's clone reads the same.
    let get = RecordOp::get(key(1), B).only(["a"]);
    assert_eq!(
        db.get(ReadTarget::Head, get.clone()).result.unwrap(),
        db.get(ReadTarget::Head, get).result.unwrap()
    );
    // An operation holding a builder error is a value too: its clone is equal and
    // fails the same way.
    let invalid = RecordOp::create(key(2), B).field("x y", CellValue::from_u32(1));
    assert_eq!(invalid.clone(), invalid);
    let message = |result: Result<RecordKey>| match result {
        Err(ApiError::InvalidArgument { message, .. }) => message,
        other => panic!("expected InvalidArgument, got {other:?}"),
    };
    let branch = db.begin().unwrap();
    assert_eq!(
        message(db.create(branch, invalid.clone()).result),
        message(db.create(branch, invalid).result)
    );
}

/// "| `record_key() -> Option<RecordKey>` | all | the key"
/// "| `budget() -> Budget` | all | the budget given at construction |"
#[test]
fn every_op_reports_its_key_and_budget() {
    fn reported<Op>(op: RecordOp<Op>) -> (RecordKey, Budget) {
        (op.record_key(), op.budget())
    }
    let u32_1 = || CellValue::from_u32(1);
    for budget in [Budget::Limited(7), Budget::Unlimited] {
        // Bare, after steps or `.only`, and holding a builder error.
        let create = || RecordOp::create(key(1), budget);
        let patch = || RecordOp::patch(key(2), budget);
        let get = || RecordOp::get(key(3), budget);
        let ops = [
            (reported(create()), key(1)),
            (
                reported(create().attribute("a", u32_1()).field("b", u32_1())),
                key(1),
            ),
            (reported(create().field("x y", u32_1())), key(1)),
            (reported(patch()), key(2)),
            (
                reported(patch().field("a", u32_1()).set("b", u32_1()).remove("c")),
                key(2),
            ),
            (reported(patch().remove("x y")), key(2)),
            (reported(get()), key(3)),
            (reported(get().only(["a"])), key(3)),
            (reported(RecordOp::delete(key(4), budget)), key(4)),
        ];
        for (reported, k) in ops {
            assert_eq!(reported, (k, budget));
        }
    }
}

/// "| `names() -> Option<&[CellName]>` | get | the projection: `None` for the full
/// record, `Some([])` for an existence check |"
/// "`.only` takes names as raw bytes (`&str` or `&[u8]`)"
#[test]
fn names_is_the_projection() {
    let get = || RecordOp::get(key(1), B);
    assert_eq!(get().names(), None);
    assert_eq!(
        get().only(std::iter::empty::<&str>()).names(),
        Some(&[][..])
    );
    // Reserved, outside the write grammar, or not UTF-8, given in bytewise order,
    // which the spec leaves open.
    let raw: [&[u8]; 4] = [b"#meta", b"b", b"x y", &[0xff]];
    let projected = get().only(raw);
    let names: Vec<&[u8]> = projected
        .names()
        .unwrap()
        .iter()
        .map(|name| name.as_bytes())
        .collect();
    assert_eq!(names, raw);
    // `&str` and `&[u8]` give the same projection.
    assert_eq!(
        get().only(["a", "#meta"]),
        get().only([b"a".as_slice(), b"#meta"])
    );
}
