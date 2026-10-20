use crate::*;
use golemdb_cells::{CellType, Width};

fn segment(name: &str) -> ImmutableDataSegment {
    ImmutableDataSegment {
        name: name.into(),
        columns: vec![ImmutableDataColumn::new("v")],
        compression: ImmutableDataCompression::None,
    }
}

#[test]
fn canonical_schema_vector_and_ids_preserve_column_order() {
    let config = ImmutableDataConfig::new(
        500_000,
        vec![ImmutableDataSegment {
            name: "s".into(),
            columns: vec![
                ImmutableDataColumn::new("n").with_type(CellType::Uint(Width::W8)),
                ImmutableDataColumn::new("b"),
            ],
            compression: ImmutableDataCompression::Lz4,
        }],
    );
    assert_eq!(
        config.encode_segments().unwrap(),
        vec![
            1, 0, 0, 0, 1, // version, one segment
            0, 0, 0, 1, b's', 1, 0, 0, 0, 2, // name, LZ4, two columns
            0, 0, 0, 1, b'n', 13, // u64
            0, 0, 0, 1, b'b', 3, // bytes
        ]
    );
    let mut reordered = config.clone();
    reordered.segments[0].columns.reverse();
    assert_ne!(
        config.encode_segments().unwrap(),
        reordered.encode_segments().unwrap()
    );

    let a = ImmutableDataConfig::new(1, vec![segment("z"), segment("a")]);
    let b = ImmutableDataConfig::new(1, vec![segment("a"), segment("z")]);
    assert_eq!(a.encode_segments().unwrap(), b.encode_segments().unwrap());
    assert_eq!(
        a.segments_by_id()
            .unwrap()
            .into_iter()
            .map(|(id, s)| (id, s.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "a"), (1, "z")]
    );
    assert_eq!(
        ImmutableDataConfig::new(u64::MAX, vec![])
            .encode_segments()
            .unwrap(),
        vec![1, 0, 0, 0, 0]
    );
}

#[test]
fn configuration_limits_cover_names_counts_duplicates_and_size() {
    assert!(matches!(
        ImmutableDataConfig::new(0, vec![]).validate(),
        Err(ImmutableDataConfigError::InvalidShardSpan)
    ));
    for name in [
        "", "#system", "@engine", "../file", "a/b", "a\\b", "a\0b", "a b", "é",
    ] {
        let mut s = segment("valid");
        s.columns[0].name = name.into();
        assert!(matches!(
            ImmutableDataConfig::new(1, vec![s]).validate(),
            Err(ImmutableDataConfigError::InvalidName { .. })
        ));
    }
    for name in ["a", "A", "$name", "a-b.c:d_9"] {
        let mut s = segment("valid");
        s.columns[0].name = name.into();
        ImmutableDataConfig::new(1, vec![s]).validate().unwrap();
    }
    assert!(matches!(
        ImmutableDataConfig::new(1, vec![segment("a"), segment("a")]).validate(),
        Err(ImmutableDataConfigError::DuplicateSegment(_))
    ));
    let mut s = segment("a");
    s.columns.push(s.columns[0].clone());
    assert!(matches!(
        ImmutableDataConfig::new(1, vec![s]).validate(),
        Err(ImmutableDataConfigError::Schema(
            ImmutableDataError::DuplicateColumn { .. }
        ))
    ));
    for count in [0, MAX_COLUMNS, MAX_COLUMNS + 1] {
        let mut s = segment("a");
        s.columns = (0..count)
            .map(|i| ImmutableDataColumn::new(format!("c{i}")))
            .collect();
        let result = ImmutableDataConfig::new(1, vec![s]).validate();
        assert_eq!(result.is_ok(), count == MAX_COLUMNS);
    }
    for count in [MAX_SEGMENTS, MAX_SEGMENTS + 1] {
        let config =
            ImmutableDataConfig::new(1, (0..count).map(|i| segment(&format!("s{i}"))).collect());
        assert_eq!(config.validate().is_ok(), count == MAX_SEGMENTS);
    }
    let mut s = segment("s");
    s.columns[0].name = "a".repeat(MAX_SCHEMA_BYTES - 20);
    let mut config = ImmutableDataConfig::new(1, vec![s]);
    assert_eq!(config.encode_segments().unwrap().len(), MAX_SCHEMA_BYTES);
    config.segments[0].columns[0].name.push('a');
    assert!(matches!(
        config.encode_segments(),
        Err(ImmutableDataConfigError::SchemaTooLarge { .. })
    ));
}

#[test]
fn segment_names_have_one_ascii_lowercase_spelling() {
    for name in [
        "a",
        "events",
        "block_headers",
        "receipts_v2",
        &"a".repeat(64),
    ] {
        let s = segment(name);
        assert_eq!(s.schema().unwrap().name(), name);
        ImmutableDataConfig::new(1, vec![s]).validate().unwrap();
    }
    for name in [
        "",
        "Events",
        "EVENTS",
        "eventS",
        "1events",
        "_events",
        "$events",
        "events.v1",
        "events-v1",
        "events:v1",
        "events ",
        "../events",
        "a/b",
        "a\\b",
        "a\0b",
        "évents",
        "ｅvents",
        &"a".repeat(65),
    ] {
        let s = segment(name);
        let error = s.schema().unwrap_err();
        assert!(
            matches!(
                error,
                ImmutableDataError::EmptySegmentName
                    | ImmutableDataError::InvalidSegmentName { .. }
            ),
            "{name:?}"
        );
        assert_eq!(
            ImmutableDataConfig::new(1, vec![s]).validate(),
            Err(ImmutableDataConfigError::Schema(error)),
            "{name:?}"
        );
    }
}
