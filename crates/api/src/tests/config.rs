use crate::*;

const PREFIX: &str = "hash_function: keccak-256\ncell_limits: {max_cell_name_len: 32, max_str_len: 64, max_bytes_len: 128}\nrecord_limits: {max_cells: 100, max_indexed_cells: 100}\n";

fn yaml(section: &str) -> String {
    format!("{PREFIX}immutable_data:\n{section}\n")
}

#[test]
fn omitted_and_null_immutable_data_match_explicit_none() {
    let expected = Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
        crate::RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
        None,
    );
    for yaml in [PREFIX.to_owned(), format!("{PREFIX}immutable_data: null\n")] {
        assert_eq!(Genesis::from_yaml(&yaml).unwrap(), expected);
    }
    assert!(matches!(
        Genesis::from_yaml(&format!(
            "{PREFIX}immutable_data: null\nimmutable_data: null\n"
        )),
        Err(OpenError::Yaml(_))
    ));
}

#[test]
fn yaml_requires_complete_enabled_configuration_and_rejects_unknown_or_duplicate_fields() {
    for section in [
        "  segments: []",
        "  shard_span: 1\n  segments: []",
        "  shard_span: 1",
        "  shard_span: 1\n  segments: []\n  unexpected: true",
        "  shard_span: 1\n  shard_span: 2\n  segments: []",
        "  shard_span: 1\n  segments: []\n  segments: []",
        "  shard_span: 0\n  segments: []",
        "  shard_span: -1\n  segments: []",
        "  shard_span: 18446744073709551616\n  segments: []",
    ] {
        assert!(Genesis::from_yaml(&yaml(section)).is_err(), "{section}");
    }
    for segment in [
        "{name: Events, compression: none, columns: [{name: value}]}",
        "{name: events.v1, compression: none, columns: [{name: value}]}",
        "{name: events, columns: [{name: value}]}",
        "{name: events, compression: none}",
        "{compression: none, columns: [{name: value}]}",
        "{name: events, name: other, compression: none, columns: [{name: value}]}",
        "{name: events, compression: none, columns: [{name: value}], extra: 1}",
        "{name: events, compression: zstd-dict, columns: [{name: value}]}",
        "{name: events, compression: none, columns: [{name: value, type: u8}]}",
        "{name: events, compression: none, columns: [{name: value, type: null}]}",
        "{name: events, compression: none, columns: [{name: value, type: bytes, type: u64}]}",
        "{name: events, compression: none, columns: [{name: value, codec: custom}]}",
        "{name: events, compression: none, columns: [{type: u64}]}",
        "{name: events, compression: none, columns: []}",
    ] {
        assert!(
            Genesis::from_yaml(&yaml(&format!("  shard_span: 1\n  segments: [{segment}]")))
                .is_err(),
            "{segment}"
        );
    }
}

#[test]
fn yaml_resolves_every_cell_type_and_default_bytes() {
    let names = [
        "bool",
        "str",
        "bytes",
        "bytes20",
        "bytes4",
        "bytes8",
        "bytes16",
        "bytes32",
        "u32",
        "u64",
        "u128",
        "u256",
        "i32",
        "i64",
        "i128",
        "i256",
        "dec32",
        "dec64",
        "dec128",
        "dec256",
        "f32",
        "f64",
        "date32",
        "timestamp64",
    ];
    let expected: Vec<_> = (0..128)
        .filter_map(|id| CellType::from_id(id).ok())
        .collect();
    for (name, expected) in names.into_iter().zip(&expected) {
        let config = Genesis::from_yaml(&yaml(&format!(
            "  shard_span: 18446744073709551615\n  segments: [{{name: events, compression: zstd, columns: [{{name: value, type: {name}}}]}}]"
        ))).unwrap();
        let immutable_data = config.immutable_data.unwrap();
        assert_eq!(&immutable_data.segments[0].columns[0].value_type, expected);
        assert_eq!(immutable_data.shard_span, u64::MAX);
    }
    assert_eq!(names.len(), expected.len());
    let implicit = yaml(
        "  shard_span: 1\n  segments: [{name: events, compression: lz4, columns: [{name: payload}]}]",
    );
    let explicit = implicit.replace("name: payload", "name: payload, type: bytes");
    assert_eq!(
        Genesis::from_yaml(&implicit).unwrap(),
        Genesis::from_yaml(&explicit).unwrap()
    );
}
