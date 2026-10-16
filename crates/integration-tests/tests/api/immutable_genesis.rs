use golemdb_api::*;
use golemdb_cells::{CellKey, CellReader, reserved, system, tables};
use golemdb_storage::{MemoryStore, Store, WriteTransaction};

const YAML: &str = r#"
hash_function: keccak-256
cell_limits: {max_cell_name_len: 1, max_str_len: 0, max_bytes_len: 0}
immutable_data:
  shard_span: 4294967297
  segments:
    - name: receipts
      compression: lz4
      columns: [{name: payload}]
    - name: headers
      compression: none
      columns:
        - {name: height, type: u64}
        - {name: body}
"#;

fn config(directory: &std::path::Path) -> OpenConfig {
    OpenConfig::new(Genesis::from_yaml(YAML).unwrap()).with_immutable_data_directory(directory)
}

#[test]
fn schema_identity_ignores_yaml_and_segment_order_but_commits_every_setting() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let files = tempfile::tempdir().unwrap();
        let mut original = config(files.path());
        original.genesis.hash_function = hash;
        let store = MemoryStore::new();
        let db = Database::from_store(store.clone(), &original).unwrap();
        let expected = *db.info();
        drop(db);
        let before = super::opening::snapshot(&store);
        let mut disabled = original.clone();
        disabled.genesis.immutable_data = None;
        assert!(matches!(
            Database::from_store(store.clone(), &disabled),
            Err(OpenError::GenesisMismatch)
        ));
        let mut equivalent = OpenConfig::new(
            Genesis::from_yaml(
                &YAML
                    .replace("{name: payload}", "{type: bytes, name: payload}")
                    .replace("{name: body}", "{name: body, type: bytes}")
                    .replace("shard_span: 4294967297", "shard_span: 4294967297 # comment"),
            )
            .unwrap(),
        );
        equivalent.immutable_data_directory = original.immutable_data_directory.clone();
        equivalent.genesis.hash_function = hash;
        equivalent
            .genesis
            .immutable_data
            .as_mut()
            .unwrap()
            .segments
            .reverse();
        let other = Database::open_memory(&equivalent.genesis).unwrap();
        assert_eq!(other.info(), &expected);
        let reopened = Database::from_store(store.clone(), &equivalent).unwrap();
        assert!(!reopened.info().created);
        assert_eq!(reopened.info().genesis_id, expected.genesis_id);
        assert_eq!(super::opening::snapshot(&store), before);

        drop(reopened);
        for change in 0..9 {
            let mut different = original.clone();
            let config = different.genesis.immutable_data.as_mut().unwrap();
            match change {
                0 => config.shard_span += 1,
                1 => config.segments[0].name.push('x'),
                2 => config.segments[0].columns[0].name.push('x'),
                3 => config.segments[0].columns[0].value_type = CellType::Str,
                4 => config.segments[0].compression = ImmutableDataCompression::Zstd,
                5 => config.segments[1].columns.reverse(),
                6 => {
                    config.segments.remove(0);
                }
                7 => config.segments.push(ImmutableDataSegment {
                    name: "extra".into(),
                    columns: vec![ImmutableDataColumn::new("v")],
                    compression: ImmutableDataCompression::None,
                }),
                _ => config.segments[1].columns[0].value_type = CellType::Uint(Width::W4),
            }
            let changed = Database::open_memory(&different.genesis).unwrap();
            assert_ne!(
                changed.info().genesis_id,
                expected.genesis_id,
                "change {change}"
            );
            assert_ne!(
                changed.info().state_root,
                expected.state_root,
                "change {change}"
            );
            assert_eq!(changed.info().index_root, expected.index_root);
            assert!(matches!(
                Database::from_store(store.clone(), &different),
                Err(OpenError::GenesisMismatch)
            ));
            assert_eq!(super::opening::snapshot(&store), before);
        }
    }
}

fn lifecycle(store: impl Store + Clone + Send + Sync + 'static, config: &OpenConfig) -> OpenInfo {
    let db = Database::from_store(store.clone(), config).unwrap();
    let info = *db.info();
    // These engine parameters exceed the configured user name/bytes limits.
    let tx = store.begin_read().unwrap();
    let reader = CellReader::new(&tx);
    let span = reader
        .get(&CellKey::new(system::PARAMS.id, reserved::SHARD_SPAN))
        .unwrap()
        .unwrap();
    assert_eq!(span.as_u64(), Some(4_294_967_297));
    assert!(!span.is_indexable());
    let schema = reader
        .get(&CellKey::new(
            system::PARAMS.id,
            reserved::IMMUTABLE_DATA_SEGMENTS,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(
        schema.as_bytes().unwrap(),
        config
            .genesis
            .immutable_data
            .as_ref()
            .unwrap()
            .encode_segments()
            .unwrap()
    );
    assert!(!schema.is_indexable());
    drop(tx);
    let branch = db.begin().unwrap();
    db.commit(branch).unwrap();
    let before = super::opening::snapshot(&store);
    drop(db);
    let reopened = Database::from_store(store.clone(), config).unwrap();
    assert_eq!(reopened.info().commit_id, CommitId::new(1));
    assert_eq!(reopened.info().genesis_id, info.genesis_id);
    assert_eq!(super::opening::snapshot(&store), before);
    info
}

#[test]
fn declarations_persist_identically_in_memory_and_mdbx_and_reopen_after_commit() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let files = tempfile::tempdir().unwrap();
        let mut config = config(files.path());
        config.genesis.hash_function = hash;
        let expected = lifecycle(MemoryStore::new(), &config);
        let dir = tempfile::tempdir().unwrap();
        let actual = lifecycle(
            golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap(),
            &config,
        );
        assert_eq!(actual, expected);
        // All original handles are gone: validate an independent disk reopen.
        let reopened = Database::open(dir.path(), &config).unwrap();
        assert_eq!(reopened.info().genesis_id, expected.genesis_id);
        assert_eq!(reopened.info().commit_id, CommitId::new(1));
    }
}

fn disabled_lifecycle(
    store: impl Store + Clone + Send + Sync + 'static,
    enabled: &OpenConfig,
) -> OpenInfo {
    let mut disabled = enabled.clone();
    disabled.genesis.immutable_data = None;
    let db = Database::from_store(store.clone(), &disabled).unwrap();
    let info = *db.info();
    assert_eq!(db.genesis().immutable_data, None);
    assert!(!disabled.immutable_data_directory.as_ref().unwrap().exists());
    let tx = store.begin_read().unwrap();
    let reader = CellReader::new(&tx);
    for name in [reserved::SHARD_SPAN, reserved::IMMUTABLE_DATA_SEGMENTS] {
        assert!(
            reader
                .get(&CellKey::new(system::PARAMS.id, name))
                .unwrap()
                .is_none()
        );
    }
    drop(tx);

    let enabled_db = Database::open_memory(&enabled.genesis).unwrap();
    assert_ne!(enabled_db.info().genesis_id, info.genesis_id);
    assert_ne!(enabled_db.info().state_root, info.state_root);
    assert!(matches!(
        Database::from_store(store.clone(), enabled),
        Err(OpenError::GenesisMismatch)
    ));
    assert!(!disabled.immutable_data_directory.as_ref().unwrap().exists());

    db.commit(db.begin().unwrap()).unwrap();
    drop(db);
    let before = super::opening::snapshot(&store);
    disabled.immutable_data_directory = None;
    let reopened = Database::from_store(store.clone(), &disabled).unwrap();
    assert_eq!(reopened.info().commit_id, CommitId::new(1));
    assert_eq!(reopened.info().genesis_id, info.genesis_id);
    assert_eq!(super::opening::snapshot(&store), before);
    info
}

#[test]
fn disabled_genesis_has_no_immutable_parameters_or_files_and_reopens() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(&dir.path().join("unused-files"));
        config.genesis.hash_function = hash;
        let expected = disabled_lifecycle(MemoryStore::new(), &config);
        let actual = disabled_lifecycle(
            golemdb_storage_mdbx::MdbxStore::open(dir.path().join("kv")).unwrap(),
            &config,
        );
        assert_eq!(actual, expected);
    }
}

#[test]
fn immutable_parameters_must_match_enabled_or_disabled_genesis() {
    for enabled in [false, true] {
        for advance in [false, true] {
            for name in [reserved::SHARD_SPAN, reserved::IMMUTABLE_DATA_SEGMENTS] {
                for delete in [false, true] {
                    if !enabled && delete {
                        continue;
                    }
                    let dir = tempfile::tempdir().unwrap();
                    let mut config = config(dir.path());
                    if !enabled {
                        config.genesis.immutable_data = None;
                    }
                    let store = MemoryStore::new();
                    let db = Database::from_store(store.clone(), &config).unwrap();
                    if advance {
                        db.commit(db.begin().unwrap()).unwrap();
                    }
                    drop(db);
                    let mut tx = store.begin_write().unwrap();
                    let key = CellKey::new(system::PARAMS.id, name).encode();
                    if delete {
                        assert!(tx.delete(tables::CELL, &key).unwrap());
                    } else {
                        tx.put(tables::CELL, &key, CellValue::from_u64(100).encoded_bytes())
                            .unwrap();
                    }
                    tx.commit().unwrap();
                    let before = super::opening::snapshot(&store);
                    assert!(matches!(
                        Database::from_store(store.clone(), &config),
                        Err(OpenError::CorruptState(_))
                    ));
                    assert_eq!(super::opening::snapshot(&store), before);
                }
            }
        }
    }
}
