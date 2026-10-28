use std::sync::Arc;

use golemdb_api::*;
use golemdb_branch::Branches;
use golemdb_cells::{CellKey, CellNameRef, CellValue, system};
use golemdb_merkle::{Blake3Hasher, HashProvider, Keccak256Hasher};
use golemdb_storage::{MemoryStore, Store, WriteTransaction};

fn config(hash: HashAlgorithm) -> OpenConfig {
    let mut pricing = PricingConfig::zero();
    pricing.host_code_version = 7;
    pricing.weights.insert("read.cell".into(), 11);
    pricing.weights.insert("write.cell_byte".into(), 3);
    pricing.weights.insert("host.storage_price".into(), 9);
    OpenConfig::new(Genesis::new(
        hash,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
        RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
        None,
        pricing,
    ))
    .with_host_schemas(vec![HostMeteringSchema {
        code_version: 7,
        weight_names: vec!["host.storage_price".into(), "host.max_lifetime".into()],
    }])
}

fn lifecycle(store: impl Store + Clone + Send + Sync + 'static, cfg: &OpenConfig) {
    let db = Database::from_store(store.clone(), cfg).unwrap();
    let api: &dyn Api = &db;
    let model = api.metering_model(None).unwrap();
    assert_eq!(
        (
            model.active.version,
            model.active.golemdb_code_version,
            model.active.host_code_version
        ),
        (1, 1, 7)
    );
    assert_eq!(model.commit_id, CommitId::GENESIS);
    assert_eq!(model.active.activation_commit, CommitId::GENESIS);
    assert_eq!(model.pending, None);
    let weights = api
        .model_weights(Some(model.commit_id), Some(model.active.version))
        .unwrap();
    assert_eq!(weights.weights["read.cell"], 11);
    assert_eq!(weights.weights["write.cell_byte"], 3);
    assert_eq!(weights.weights["write.record.create"], 0);
    assert_eq!(weights.weights["host.storage_price"], 9);
    assert!(!weights.weights.contains_key("host.max_lifetime"));
    assert!(Arc::ptr_eq(
        &weights.weights,
        &db.clone().model_weights(None, None).unwrap().weights
    ));
    assert!(matches!(
        api.model_weights(None, Some(99)),
        Err(ApiError::NotFound)
    ));
    assert!(matches!(
        api.metering_model(Some(CommitId::new(1))),
        Err(ApiError::CommitUnavailable { .. })
    ));
    assert!(matches!(
        api.model_weights(Some(CommitId::new(1)), None),
        Err(ApiError::CommitUnavailable { .. })
    ));

    let branch = api.begin().unwrap();
    let created = api.create(
        branch,
        RecordOp::create(RecordKey([1; 32]), Budget::Limited(474))
            .field("x", CellValue::from_u64(12)),
    );
    // 140 system-cell bytes + 18 user-cell bytes, at the persisted price of 3.
    assert_eq!(created.receipt.cost, 474);
    assert!(created.result.is_ok());
    assert_eq!(
        api.model_weights(None, None).unwrap().commit_id,
        CommitId::GENESIS
    );
    api.commit(branch).unwrap();
    let later = api.model_weights(None, None).unwrap();
    assert_eq!(later.commit_id, CommitId::new(1));
    assert_eq!(weights.commit_id, CommitId::GENESIS);
    assert!(Arc::ptr_eq(&weights.weights, &later.weights));
    assert!(matches!(
        api.model_weights(Some(CommitId::GENESIS), None),
        Err(ApiError::CommitUnavailable { .. })
    ));
    assert!(matches!(
        api.metering_model(Some(CommitId::GENESIS)),
        Err(ApiError::CommitUnavailable { .. })
    ));
    let before = super::opening::snapshot(&store);
    drop(db);
    let reopened = Database::from_store(store.clone(), cfg).unwrap();
    assert_eq!(reopened.model_weights(None, None).unwrap(), later);
    assert_eq!(super::opening::snapshot(&store), before);
    assert_eq!(
        reopened.metering_model(None).unwrap().commit_id,
        CommitId::new(1)
    );
    let branch = reopened.begin().unwrap();
    let created = reopened.create(
        branch,
        RecordOp::create(RecordKey([2; 32]), Budget::Limited(474))
            .field("x", CellValue::from_u64(12)),
    );
    created.result.unwrap();
    assert_eq!(created.receipt.cost, 474);
    assert_eq!(created.receipt.priced_at, Some(CommitId::new(1)));
    reopened.discard(branch).unwrap();
}

#[test]
fn persisted_host_and_database_prices_reopen_and_share_cached_weights() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let config = config(hash);
        lifecycle(MemoryStore::new(), &config);
        let dir = tempfile::tempdir().unwrap();
        lifecycle(
            golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap(),
            &config,
        );
    }
}

#[test]
fn initial_prices_affect_genesis_identity_but_local_capabilities_do_not() {
    let config = config(HashAlgorithm::Keccak256);
    let store = MemoryStore::new();
    let original = Database::from_store(store.clone(), &config).unwrap();
    let mut changed = config.clone();
    changed
        .genesis
        .metering
        .weights
        .insert("host.storage_price".into(), 10);
    let other = Database::from_store(MemoryStore::new(), &changed).unwrap();
    assert_ne!(original.info().genesis_id, other.info().genesis_id);
    assert_ne!(original.info().state_root, other.info().state_root);
    assert!(matches!(
        Database::from_store(store.clone(), &changed),
        Err(OpenError::GenesisMismatch)
    ));
    let mut compatible = config.clone();
    compatible.host_schemas[0].weight_names.reverse();
    compatible.host_schemas.push(HostMeteringSchema {
        code_version: 8,
        weight_names: vec!["host.other".into()],
    });
    assert_eq!(
        Database::from_store(store, &compatible)
            .unwrap()
            .info()
            .genesis_id,
        original.info().genesis_id
    );
}

#[test]
fn invalid_initial_model_is_rejected_before_genesis_publication() {
    let cfg = config(HashAlgorithm::Keccak256);
    for change in 0..5 {
        let mut bad = cfg.clone();
        match change {
            0 => bad.genesis.metering.golemdb_code_version = 99,
            1 => bad.host_schemas.clear(),
            2 => {
                bad.genesis.metering.weights.insert("host.typo".into(), 1);
            }
            3 => {
                bad.genesis.metering.weights.insert("write.typo".into(), 1);
            }
            _ => bad.host_schemas[0].weight_names.push("read.cell".into()),
        }
        let store = MemoryStore::new();
        assert!(matches!(
            Database::from_store(store.clone(), &bad),
            Err(OpenError::Metering(_))
        ));
        assert!(store.begin_write().unwrap().is_empty().unwrap());
    }
}

fn mutate<S: Store + Clone, H: HashProvider>(store: S, hash: H, key: CellKey, value: CellValue) {
    let branches = Branches::new(store, hash, None, None).unwrap();
    let b = branches.begin().unwrap();
    branches
        .write(b, |writer| {
            writer.put(key, value);
            Ok::<_, golemdb_branch::BranchError>(())
        })
        .unwrap();
    branches.commit(b).unwrap();
}

#[test]
fn reopening_loads_committed_weights_instead_of_replacing_them_with_genesis() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let cfg = config(hash);
        let store = MemoryStore::new();
        drop(Database::from_store(store.clone(), &cfg).unwrap());
        // Simulate a previously committed admin patch. No live engine shares this
        // low-level writer; production admin mutation/refresh is iteration 8.
        let name = [1u64.to_be_bytes().as_slice(), b"read.cell"].concat();
        let key = CellKey::new(system::MODEL_WEIGHT.id, CellNameRef::raw(&name));
        match hash {
            HashAlgorithm::Keccak256 => mutate(
                store.clone(),
                Keccak256Hasher,
                key,
                CellValue::from_u64(999),
            ),
            HashAlgorithm::Blake3 => {
                mutate(store.clone(), Blake3Hasher, key, CellValue::from_u64(999))
            }
        }
        let db = Database::from_store(store, &cfg).unwrap();
        assert_eq!(
            db.model_weights(None, None).unwrap().weights["read.cell"],
            999
        );
        assert_eq!(db.genesis().metering.weights["read.cell"], 11);
        let branch = db.begin().unwrap();
        let key = RecordKey([3; 32]);
        db.create(branch, RecordOp::create(key, Budget::Unlimited))
            .result
            .unwrap();
        // An empty patch reads #meta and #key at the committed patched price,
        // not the genesis value or the former hardcoded zero schedule.
        let patched = db.patch(branch, RecordOp::patch(key, Budget::Limited(1998)));
        patched.result.unwrap();
        assert_eq!(patched.receipt.cost, 1998);
        assert_eq!(patched.receipt.ledger.count("read.cell"), 2);
        assert_eq!(patched.receipt.priced_at, Some(CommitId::new(1)));
    }
}

#[test]
fn reopening_refuses_malformed_or_unsupported_active_models() {
    for value in [
        CellValue::from_bytes(&[0; 23]),
        CellValue::from_bytes(
            &[0u64.to_be_bytes(), 99u64.to_be_bytes(), 0u64.to_be_bytes()].concat(),
        ),
        CellValue::from_bytes(
            &[0u64.to_be_bytes(), 1u64.to_be_bytes(), 99u64.to_be_bytes()].concat(),
        ),
    ] {
        let cfg = config(HashAlgorithm::Keccak256);
        let store = MemoryStore::new();
        drop(Database::from_store(store.clone(), &cfg).unwrap());
        let key = CellKey::new(
            system::METERING_MODEL.id,
            CellNameRef::raw(&1u64.to_be_bytes()),
        );
        mutate(store.clone(), Keccak256Hasher, key, value);
        assert!(matches!(
            Database::from_store(store, &cfg),
            Err(OpenError::Metering(_))
        ));
    }
}

#[test]
fn reopening_requires_active_host_code_not_retired_genesis_host_code() {
    let mut cfg = config(HashAlgorithm::Keccak256);
    let store = MemoryStore::new();
    drop(Database::from_store(store.clone(), &cfg).unwrap());
    let key = CellKey::new(
        system::METERING_MODEL.id,
        CellNameRef::raw(&2u64.to_be_bytes()),
    );
    mutate(
        store.clone(),
        Keccak256Hasher,
        key,
        CellValue::from_bytes(
            &[2u64.to_be_bytes(), 1u64.to_be_bytes(), 8u64.to_be_bytes()].concat(),
        ),
    );
    let db = Database::from_store(store.clone(), &cfg).unwrap();
    let model = db.metering_model(None).unwrap();
    assert_eq!(model.active.version, 1);
    assert_eq!(model.active.activation_commit, CommitId::GENESIS);
    assert_eq!(
        model.pending,
        Some(MeteringModelMetadata {
            version: 2,
            activation_commit: CommitId::new(2),
            golemdb_code_version: 1,
            host_code_version: 8,
        })
    );
    // Reopening at activation selects the new schema without requiring the old
    // host implementation. Live activation/cache replacement remains deferred.
    db.commit(db.begin().unwrap()).unwrap();
    let branch = db.begin().unwrap();
    let blocked = db.create(
        branch,
        RecordOp::create(RecordKey([4; 32]), Budget::Unlimited),
    );
    assert!(matches!(blocked.result, Err(ApiError::Internal { .. })));
    assert_eq!(blocked.receipt, Receipt::default());
    assert_eq!(db.branch_info(branch).unwrap().version, 0);
    drop(db);
    cfg.host_schemas = vec![HostMeteringSchema {
        code_version: 8,
        weight_names: vec!["host.limit".into()],
    }];
    let db = Database::from_store(store, &cfg).unwrap();
    let model = db.metering_model(None).unwrap();
    assert_eq!(model.active.version, 2);
    assert_eq!(model.active.activation_commit, CommitId::new(2));
    assert_eq!(model.active.host_code_version, 8);
    assert_eq!(model.pending, None);
    assert!(db.model_weights(None, None).unwrap().weights.is_empty());
    assert_eq!(
        db.model_weights(None, Some(1)).unwrap().weights["host.storage_price"],
        9
    );
}

#[test]
fn host_weight_keys_must_fit_backend_even_though_user_caps_do_not_apply() {
    let dir = tempfile::tempdir().unwrap();
    let store = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
    let mut cfg = config(HashAlgorithm::Keccak256);
    let name = format!("host.{}", "x".repeat(store.max_key_size()));
    cfg.genesis.metering.weights.insert(name.clone(), 1);
    cfg.host_schemas[0].weight_names.push(name);
    assert!(matches!(
        Database::from_store(store.clone(), &cfg),
        Err(OpenError::InvalidConfig(_))
    ));
    assert!(store.begin_write().unwrap().is_empty().unwrap());
}
