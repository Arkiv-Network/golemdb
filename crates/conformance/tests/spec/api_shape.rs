//! Shape of the API.
//!
//! Where the spec is stale or silent, so nothing here tests it:
//! - The trait's methods: the spec lists record, branch and immutable-data calls;
//!   the code also has `query`, `count`, `metering_model`, `model_weights`,
//!   `immutable_data_schema` and `immutable_data_truncate`, has no
//!   `immutable_data_range_of`, and its immutable-data reads take read options.
//!   Each call is, or will be, tested or listed in its own section's module.
//! - The trait block's "Immutable data: interface only, every call returns
//!   NotImplemented": the code implements those calls.
//!
//! Tested elsewhere:
//! - "no direct access to reserved cells": writes to reserved keys are `Reserved`,
//!   in the `create`, `patch` and `delete` modules; reserved cell names on user
//!   records are refused in `data_model::names_follow_the_grammar` and
//!   `patch::reserved_cell_names_cannot_be_patched`; reads go through `get`, as
//!   `data_model` tests.
//! - `info()`'s roots: `data_model` checks them against `#roots`.
//!
//! Not covered here:
//! - `info()`'s `genesis_id`: its value belongs to the Opening section's "What
//!   genesis writes".
//! - "only `commit` writes to the store", for writes that leave nothing readable,
//!   such as unreachable trie nodes: seeing them needs the store, which the API does
//!   not expose.
//! - That the store is hidden: the only other store reachable through the API is
//!   the in-memory one, which these tests do not use.
//! - "one dynamic dispatch", "hashing within a call is static", "no raw store
//!   writes", and that adding a required method breaks implementations: properties
//!   of the types, not behaviour.

use std::sync::Arc;

use golemdb_conformance::*;

/// `key(1)` with `a`, a `u32` field 1, created and committed through `api`.
fn create_1(api: &dyn Api) {
    let branch = api.begin().unwrap();
    api.create(
        branch,
        RecordOp::create(key(1), B).field("a", CellValue::from_u32(1)),
    )
    .result
    .unwrap();
    api.commit(branch).unwrap();
}

/// "One synchronous, dyn-compatible trait, `Api`, implemented by the cloneable
/// `Database` facade. Consumers hold `&dyn Api` or `Arc<dyn Api + Send + Sync>`"
/// "`Result<T>` is `std::result::Result<T, ApiError>`."
#[test]
fn api_is_held_as_a_trait_object() {
    let db = TestDb::new();
    // Through `&dyn Api`.
    let api: &dyn Api = &*db;
    create_1(api);
    // Through `Arc<dyn Api + Send + Sync>`, on another thread.
    let shared: Arc<dyn Api + Send + Sync> = Arc::new((*db).clone());
    std::thread::spawn(move || {
        let branch = shared.begin().unwrap();
        shared
            .patch(
                branch,
                RecordOp::patch(key(1), B).field("a", CellValue::from_u32(2)),
            )
            .result
            .unwrap();
        shared.commit(branch).unwrap();
    })
    .join()
    .unwrap();
    assert_eq!(
        db.read(ReadTarget::Head, key(1)).cells["a".as_bytes()],
        CellValue::from_u32(2)
    );
    // The crate's `Result` is the standard one.
    let head: Result<CommitId> = api.head();
    let head: std::result::Result<CommitId, ApiError> = head;
    assert_eq!(head.unwrap(), CommitId::new(2));
}

/// "the trait imposes no threading bounds."
#[test]
fn api_has_no_threading_bounds() {
    // The call picks an impl by inference: it is ambiguous, and fails to compile,
    // if `dyn Api` is `Send` or `Sync`, so only while `Api` has neither bound.
    trait Unbound<Marker> {
        fn check() {}
    }
    impl<T: ?Sized> Unbound<()> for T {}
    impl<T: ?Sized + Send> Unbound<u8> for T {}
    impl<T: ?Sized + Sync> Unbound<u16> for T {}
    <dyn Api as Unbound<_>>::check();
}

/// "`genesis() -> &Genesis`: the deployment settings validated at opening."
/// "`info() -> &OpenInfo`: a snapshot of the opening, `{created, commit_id,
/// state_root, index_root, genesis_id}`. It does not follow the head; call `head()`
/// for that."
#[test]
fn genesis_and_info_describe_the_opening() {
    let db = TestDb::new();
    assert_eq!(*db.genesis(), genesis(LIMITS, PricingConfig::zero()));
    let opened = *db.info();
    assert!(opened.created);
    assert_eq!(opened.commit_id, CommitId::GENESIS);
    // After a commit, the head moves on and the info, on any clone, does not.
    create_1(&*db);
    assert_eq!(db.head().unwrap(), CommitId::new(1));
    assert_eq!(*db.info(), opened);
    assert_eq!(*(*db).clone().info(), opened);
    assert_eq!((*db).clone().genesis(), db.genesis());
}

/// "**Clones share state.** All clones of a `Database` share one branch registry,
/// so a branch begun through one clone works through any other and across
/// threads."
#[test]
fn clones_share_branches() {
    let db = TestDb::new();
    let (first, second, third) = ((*db).clone(), (*db).clone(), (*db).clone());
    let a = |branch| db.read(ReadTarget::Branch(branch), key(1)).cells["a".as_bytes()].clone();
    // Begun through one clone, written through another.
    let branch = first.begin().unwrap();
    second
        .create(
            branch,
            RecordOp::create(key(1), B).field("a", CellValue::from_u32(1)),
        )
        .result
        .unwrap();
    // A checkpoint through one clone is what a rollback through another returns to.
    first.checkpoint(branch).unwrap();
    second
        .patch(
            branch,
            RecordOp::patch(key(1), B).field("a", CellValue::from_u32(2)),
        )
        .result
        .unwrap();
    assert_eq!(a(branch), CellValue::from_u32(2));
    third.rollback(branch).unwrap();
    assert_eq!(a(branch), CellValue::from_u32(1));
    // Sealed through one clone, it is sealed for every other.
    second.seal(branch).unwrap();
    assert!(third.branch_info(branch).unwrap().sealed);
    assert_err!(
        first
            .patch(
                branch,
                RecordOp::patch(key(1), B).field("a", CellValue::from_u32(3)),
            )
            .result,
        ApiError::Sealed
    );
    // Committed on another thread, it is consumed for every clone.
    let committed = std::thread::spawn(move || third.commit(branch).unwrap())
        .join()
        .unwrap();
    assert_err!(first.commit(branch), ApiError::HandleInvalid);
    for clone in [&first, &second, &*db] {
        assert_eq!(clone.head().unwrap(), committed);
        assert_eq!(
            clone
                .get(ReadTarget::Head, RecordOp::get(key(1), B))
                .result
                .unwrap()
                .cells["a".as_bytes()],
            CellValue::from_u32(1)
        );
    }
    // Discarded through one clone, it is gone for every other.
    let discarded = first.begin().unwrap();
    second.discard(discarded).unwrap();
    assert_err!(first.branch_info(discarded), ApiError::HandleInvalid);
}

/// "Open branches live in memory: dropping the last clone discards them"
#[test]
fn dropping_a_clone_that_is_not_the_last_keeps_branches() {
    let db = TestDb::new();
    let clone = (*db).clone();
    let branch = clone.begin().unwrap();
    clone
        .create(
            branch,
            RecordOp::create(key(1), B).field("a", CellValue::from_u32(1)),
        )
        .result
        .unwrap();
    drop(clone);
    // The branch, and what it holds, are still there.
    assert!(db.branch_info(branch).is_ok());
    assert_eq!(
        db.read(ReadTarget::Branch(branch), key(1)).cells["a".as_bytes()],
        CellValue::from_u32(1)
    );
    db.commit(branch).unwrap();
}

/// "Open branches live in memory: dropping the last clone discards them; only
/// `commit` writes to the store."
/// "`info() -> &OpenInfo`: a snapshot of the opening"
#[test]
fn only_commits_outlive_the_last_clone() {
    let dir = tempfile::tempdir().unwrap();
    let open = |mode| {
        Database::open(
            dir.path(),
            &OpenConfig::new(genesis(LIMITS, PricingConfig::zero())).with_mode(mode),
        )
        .unwrap()
    };
    let head_reserved = |db: &Database| {
        RESERVED_NAMES.map(|name| {
            db.get(ReadTarget::Head, RecordOp::get(reserved_key(name), B))
                .result
                .unwrap()
        })
    };
    let db = open(OpenMode::CreateNew);
    let genesis_id = db.info().genesis_id;
    let clone = db.clone();
    let first = db.begin().unwrap();
    db.create(
        first,
        RecordOp::create(key(1), B).field("a", CellValue::from_u32(1)),
    )
    .result
    .unwrap();
    let sealed = db.seal(first).unwrap();
    db.commit(first).unwrap();
    let reserved = head_reserved(&db);
    // A second branch, written, checkpointed and sealed, but never committed.
    let branch = clone.begin().unwrap();
    clone
        .create(
            branch,
            RecordOp::create(key(2), B).field("a", CellValue::from_u32(2)),
        )
        .result
        .unwrap();
    clone.checkpoint(branch).unwrap();
    clone.seal(branch).unwrap();
    drop((db, clone));
    // Reopened, the store holds the commit and nothing of the open branch.
    let db = open(OpenMode::ExistingOnly);
    // No branch is begun yet, so the old ID cannot name a new one.
    assert_err!(db.branch_info(branch), ApiError::HandleInvalid);
    assert_eq!(db.head().unwrap(), CommitId::new(1));
    let get = |k| db.get(ReadTarget::Head, RecordOp::get(k, B)).result;
    assert_eq!(
        get(key(1)).unwrap().cells["a".as_bytes()],
        CellValue::from_u32(1)
    );
    assert_err!(get(key(2)), ApiError::NotFound);
    assert_eq!(head_reserved(&db), reserved);
    // The genesis and info now describe this opening.
    assert_eq!(*db.genesis(), genesis(LIMITS, PricingConfig::zero()));
    assert_eq!(
        *db.info(),
        OpenInfo {
            created: false,
            commit_id: CommitId::new(1),
            state_root: sealed.state_root,
            index_root: sealed.index_root,
            genesis_id,
        }
    );
}

/// "**Dispatch.** The store and hash function are chosen at opening and hidden:
/// `Database` has no type parameters."
#[test]
fn hash_is_hidden_behind_one_type() {
    let open = |hash| {
        let dir = tempfile::tempdir().unwrap();
        let genesis = Genesis::new(hash, LIMITS, RECORD_LIMITS, None, PricingConfig::zero());
        let db = Database::open(
            dir.path(),
            &OpenConfig::new(genesis).with_mode(OpenMode::CreateNew),
        )
        .unwrap();
        (db, dir)
    };
    // Default type parameters would still let `Database` be named bare; one array
    // holds both only if the hash is not in the type.
    let dbs: [(Database, _); 2] = [open(HashAlgorithm::Keccak256), open(HashAlgorithm::Blake3)];
    let [keccak, blake3] = dbs.map(|(db, _dir)| {
        let api: &dyn Api = &db;
        let branch = api.begin().unwrap();
        api.create(
            branch,
            RecordOp::create(key(1), B).field("a", CellValue::from_u32(1)),
        )
        .result
        .unwrap();
        api.seal(branch).unwrap().state_root
    });
    // The hash in force is the one given at opening.
    assert_ne!(keccak, blake3);
}
