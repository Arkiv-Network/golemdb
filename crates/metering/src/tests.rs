use std::{collections::BTreeMap, sync::Arc};

use golemdb_cells::{CellKey, CellNameRef, CellReader, CellValue, system, tables};
use golemdb_storage::{MemoryStore, Store, WriteTransaction};

use crate::*;

fn store(initial: &PricingConfig) -> MemoryStore {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    let metadata = ModelMetadata {
        version: 1,
        activation_commit: 0,
        golemdb_code_version: initial.golemdb_code_version,
        host_code_version: initial.host_code_version,
    };
    for (key, value) in model_cells(&metadata, &initial.weights) {
        tx.put(tables::CELL, &key.encode(), value.encoded_bytes())
            .unwrap();
    }
    tx.commit().unwrap();
    store
}

fn load(
    store: &MemoryStore,
    commit: u64,
    host: &[HostMeteringSchema],
) -> Result<PricingProvider, ModelError> {
    PricingProvider::load(&CellReader::new(&store.begin_read().unwrap()), commit, host)
}

fn put(store: &MemoryStore, record: u64, name: &[u8], value: CellValue) {
    let mut tx = store.begin_write().unwrap();
    tx.put(
        tables::CELL,
        &CellKey::new(record, CellNameRef::raw(name)).encode(),
        value.encoded_bytes(),
    )
    .unwrap();
    tx.commit().unwrap();
}

fn model(store: &MemoryStore, version: u64, activation: u64, golemdb: u64, host: u64) {
    let value = [activation, golemdb, host]
        .into_iter()
        .flat_map(u64::to_be_bytes)
        .collect::<Vec<_>>();
    put(
        store,
        system::METERING_MODEL.id,
        &version.to_be_bytes(),
        CellValue::from_bytes(&value),
    );
}

#[test]
fn canonical_cells_match_the_design_and_ignore_user_name_caps() {
    let initial = PricingConfig {
        golemdb_code_version: 1,
        host_code_version: 9,
        weights: BTreeMap::from([("host.storage_price".into(), u64::MAX)]),
    };
    // The codec also serves models installed after genesis; callers choose V/A.
    let metadata = ModelMetadata {
        version: 2,
        activation_commit: 100,
        golemdb_code_version: initial.golemdb_code_version,
        host_code_version: initial.host_code_version,
    };
    let cells = model_cells(&metadata, &initial.weights);
    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0].0.record_id(), 32);
    assert_eq!(cells[0].0.name().as_bytes(), 2u64.to_be_bytes());
    assert_eq!(
        cells[0].1.as_bytes().unwrap(),
        [100u64.to_be_bytes(), 1u64.to_be_bytes(), 9u64.to_be_bytes()].concat()
    );
    assert_eq!(cells[1].0.record_id(), 33);
    assert_eq!(
        cells[1].0.name().as_bytes(),
        [2u64.to_be_bytes().as_slice(), b"host.storage_price"].concat()
    );
    assert_eq!(cells[1].1.as_u64(), Some(u64::MAX));
    assert!(cells.iter().all(|(_, value)| !value.is_indexable()));
}

#[test]
fn snapshots_share_prices_across_commits_and_keep_their_own_commit() {
    let provider = load(&store(&PricingConfig::zero()), 7, &[]).unwrap();
    let first = provider.snapshot(7).unwrap();
    let later = provider.clone().snapshot(100).unwrap();
    assert_eq!(first.commit_id, 7);
    assert_eq!(later.commit_id, 100);
    assert!(Arc::ptr_eq(&first.model, &later.model));
    assert!(Arc::ptr_eq(
        &provider.model_weights(None).unwrap(),
        &provider.model_weights(Some(1)).unwrap()
    ));
    assert!(matches!(
        provider.snapshot(6),
        Err(ModelError::ReloadRequired(6))
    ));
    assert!(matches!(
        provider.model_weights(Some(2)),
        Err(ModelError::ModelNotFound(2))
    ));
}

#[test]
fn missing_weights_remain_unpriced_while_zero_and_host_values_are_preserved() {
    let host = [HostMeteringSchema {
        code_version: 4,
        weight_names: vec!["host.storage_price".into(), "host.limit".into()],
    }];
    let initial = PricingConfig {
        golemdb_code_version: 1,
        host_code_version: 4,
        weights: BTreeMap::from([
            ("read.cell".into(), 0),
            ("host.storage_price".into(), u64::MAX),
        ]),
    };
    initial.validate(&host).unwrap();
    let provider = load(&store(&initial), 0, &host).unwrap();
    let snapshot = provider.snapshot(0).unwrap();
    let pricing = &snapshot.model;
    assert_eq!(pricing.schedule.weight("read.cell"), Some(0));
    assert_eq!(
        pricing.schedule.weight("host.storage_price"),
        Some(u64::MAX)
    );
    assert_eq!(pricing.schedule.weight("host.limit"), None);
    assert_eq!(
        provider.model_weights(None).unwrap()["host.storage_price"],
        u64::MAX
    );
    let mut meter = WriteMeter::new(&pricing.schedule, INITIAL_DEPTHS, Budget::Unlimited);
    meter.run_step(PlanningStep::CellRead, || ()).unwrap();
    let receipt = meter.finish().unwrap();
    assert_eq!(receipt.cost, 0);
    assert_eq!(receipt.ledger.count("host.storage_price"), 0);
    let mut meter = WriteMeter::new(&pricing.schedule, INITIAL_DEPTHS, Budget::Unlimited);
    assert!(
        meter
            .run_step(PlanningStep::AdmissionBase, || panic!("unpriced action"))
            .is_err()
    );
}

#[test]
fn configuration_rejects_undeclared_names_and_bad_host_capabilities() {
    let mut initial = PricingConfig::zero();
    for name in [
        "host.price",
        "write.typo",
        "read.typo",
        "host.",
        "host.price\0",
        "$host.price",
    ] {
        initial.weights.insert(name.into(), 1);
        assert!(initial.validate(&[]).is_err(), "{name}");
        initial.weights.remove(name);
    }
    for host in [
        vec![HostMeteringSchema {
            code_version: 0,
            weight_names: vec![],
        }],
        vec![HostMeteringSchema {
            code_version: 1,
            weight_names: vec!["read.cell".into()],
        }],
        vec![HostMeteringSchema {
            code_version: 1,
            weight_names: vec!["host.x".into(), "host.x".into()],
        }],
        vec![
            HostMeteringSchema {
                code_version: 1,
                weight_names: vec![],
            },
            HostMeteringSchema {
                code_version: 1,
                weight_names: vec![],
            },
        ],
    ] {
        assert!(initial.validate(&host).is_err());
    }
    initial.host_code_version = 7;
    assert!(matches!(
        initial.validate(&[]),
        Err(ModelError::UnsupportedHostCode(7))
    ));
    initial.host_code_version = 0;
    initial.golemdb_code_version = 2;
    assert!(matches!(
        initial.validate(&[]),
        Err(ModelError::UnsupportedGolemDbCode(2))
    ));
}

#[test]
fn activation_selection_never_falls_back_from_unsupported_active_code() {
    let store = store(&PricingConfig::zero());
    model(&store, 2, 10, 99, 0);
    let provider = load(&store, 9, &[]).unwrap();
    assert_eq!(provider.snapshot(9).unwrap().model.metadata.version, 1);
    assert_eq!(provider.pending().unwrap().version, 2);
    assert!(matches!(
        provider.snapshot(10),
        Err(ModelError::ReloadRequired(10))
    ));
    assert!(matches!(
        load(&store, 10, &[]),
        Err(ModelError::UnsupportedGolemDbCode(99))
    ));
    model(&store, 2, 10, 1, 99);
    assert!(matches!(
        load(&store, 10, &[]),
        Err(ModelError::UnsupportedHostCode(99))
    ));
    model(&store, 2, 10, 1, 0);
    let active = load(&store, 10, &[]).unwrap();
    assert_eq!(active.snapshot(10).unwrap().model.metadata.version, 2);
    assert!(active.model_weights(None).unwrap().is_empty()); // no inheritance from v1
}

#[test]
fn malformed_persisted_models_and_weights_fail_loading() {
    assert!(load(&MemoryStore::new(), 0, &[]).is_err());
    for value in [CellValue::from_bytes(&[0; 23]), CellValue::from_u64(0)] {
        let store = store(&PricingConfig::zero());
        put(&store, 32, &1u64.to_be_bytes(), value);
        assert!(load(&store, 0, &[]).is_err());
    }
    for (key, value) in [
        (vec![0; 8], CellValue::from_u64(1)),
        (
            [1u64.to_be_bytes().as_slice(), &[0xff]].concat(),
            CellValue::from_u64(1),
        ),
        (
            [2u64.to_be_bytes().as_slice(), b"read.cell"].concat(),
            CellValue::from_u64(1),
        ),
        (
            [1u64.to_be_bytes().as_slice(), b"read.cell"].concat(),
            CellValue::from_u32(1),
        ),
        (
            [1u64.to_be_bytes().as_slice(), b"read.cell"].concat(),
            CellValue::from_str("free"),
        ),
    ] {
        let store = store(&PricingConfig::zero());
        put(&store, 33, &key, value);
        assert!(load(&store, 0, &[]).is_err());
    }
}

#[test]
fn loading_excludes_unknown_names_from_pricing_but_preserves_them_for_inspection() {
    let store = store(&PricingConfig::zero());
    let unknown = ["write.typo", "read.typo", "host.typo"];
    for name in unknown {
        put(
            &store,
            33,
            &[1u64.to_be_bytes().as_slice(), name.as_bytes()].concat(),
            CellValue::from_u64(100),
        );
    }
    let provider = load(&store, 0, &[]).unwrap();
    let model = provider.snapshot(0).unwrap().model;
    let stored = provider.model_weights(None).unwrap();
    for name in unknown {
        assert_eq!(stored[name], 100);
        assert_eq!(model.schedule.weight(name), None);
        assert_eq!(
            model
                .schedule
                .price(&Ledger::from_counts([(name.into(), 1)])),
            Err(PricingError::Unpriced(name.into()))
        );
    }
    let mut meter = WriteMeter::new(&model.schedule, INITIAL_DEPTHS, Budget::Limited(0));
    meter.run_step(PlanningStep::AdmissionBase, || ()).unwrap();
    let receipt = meter.finish().unwrap();
    assert_eq!(receipt.cost, 0);
    for name in unknown {
        assert_eq!(receipt.ledger.count(name), 0);
    }
}

#[test]
fn pending_weights_are_inspectable_without_knowing_their_future_schema() {
    let store = store(&PricingConfig::zero());
    model(&store, 2, 10, 99, 99);
    for name in ["read.future", "host.future"] {
        put(
            &store,
            33,
            &[2u64.to_be_bytes().as_slice(), name.as_bytes()].concat(),
            CellValue::from_u64(42),
        );
    }
    let provider = load(&store, 0, &[]).unwrap();
    let snapshot = provider.snapshot(0).unwrap();
    let stored = provider.model_weights(Some(2)).unwrap();
    for name in ["read.future", "host.future"] {
        assert_eq!(stored[name], 42);
        assert_eq!(snapshot.model.schedule.weight(name), None);
    }
}
