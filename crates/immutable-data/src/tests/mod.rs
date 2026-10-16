mod config;
mod storage;

use crate::{
    FromImmutableDataValue, ImmutableDataAddress, ImmutableDataColumn, ImmutableDataError,
    ImmutableDataKey, ImmutableDataOrdinal, ImmutableDataReadOptions, ImmutableDataRow,
    ImmutableDataSchema,
};
use golemdb_cells::{CellKind, CellType, CellValue, FloatWidth, Width};
use proptest::prelude::*;

fn schema_for(values: &[CellValue]) -> ImmutableDataSchema {
    ImmutableDataSchema::new(
        "events",
        values
            .iter()
            .enumerate()
            .map(|(i, value)| {
                ImmutableDataColumn::new(format!("c{i}")).with_type(value.cell_type())
            })
            .collect(),
    )
    .unwrap()
}

#[test]
fn every_cell_type_round_trips_payload_only_and_normalizes_index_flags() {
    let values = vec![
        CellValue::from_bool(true),
        CellValue::from_str("zażółć"),
        CellValue::from_bytes(b"\x00\xff"),
        CellValue::from_bytes20([20; 20]),
        CellValue::from_bytes4([4; 4]),
        CellValue::from_bytes8([8; 8]),
        CellValue::from_bytes16([16; 16]),
        CellValue::from_bytes32([32; 32]),
        CellValue::from_u32(u32::MAX),
        CellValue::from_u64(u64::MAX),
        CellValue::from_u128(u128::MAX),
        CellValue::from_u256_be([255; 32]),
        CellValue::from_i32(i32::MIN),
        CellValue::from_i64(i64::MIN),
        CellValue::from_i128(i128::MIN),
        CellValue::from_i256_be([255; 32]),
        CellValue::from_dec32_unscaled(-1234),
        CellValue::from_dec64_unscaled(-1234567),
        CellValue::from_dec128_unscaled(i128::MIN),
        CellValue::from_dec256_unscaled_be([255; 32]),
        CellValue::from_f32(-1.5).unwrap(),
        CellValue::from_f64(f64::INFINITY).unwrap(),
        CellValue::from_date32(-1),
        CellValue::from_timestamp64(-1234567),
    ];
    // Fail if cells gains a type without adding an immutable-data round trip.
    assert_eq!(
        values.iter().map(CellValue::cell_type).collect::<Vec<_>>(),
        (0..128)
            .filter_map(|id| CellType::from_id(id).ok())
            .collect::<Vec<_>>()
    );
    let schema = schema_for(&values);
    let input = values
        .iter()
        .cloned()
        .map(|value| {
            if value.cell_type() == CellType::Bytes {
                value
            } else {
                value.with_kind(CellKind::Attribute).unwrap()
            }
        })
        .collect();
    let row = ImmutableDataRow::new(input);
    schema.validate_row(&row).unwrap();
    assert_eq!(row.values(), &values);
    let payloads = row.payloads().collect::<Vec<_>>();
    for (value, payload) in values.iter().zip(&payloads) {
        assert_eq!(*payload, &value.encoded_bytes()[1..]);
    }
    assert_eq!(schema.decode_row(&payloads).unwrap().values(), row.values());
    assert_eq!(row.clone().into_values(), values);
}

#[test]
fn schema_defaults_and_invalid_declarations() {
    let bytes = ImmutableDataColumn::new("payload");
    assert_eq!(bytes.value_type, CellType::Bytes);
    let schema = ImmutableDataSchema::new("events", vec![bytes.clone()]).unwrap();
    assert_eq!(schema.name(), "events");
    assert_eq!(schema.columns(), std::slice::from_ref(&bytes));
    assert_eq!(
        ImmutableDataSchema::new("", vec![bytes.clone()]),
        Err(ImmutableDataError::EmptySegmentName)
    );
    assert_eq!(
        ImmutableDataSchema::new("events", vec![]),
        Err(ImmutableDataError::EmptySchema)
    );
    assert_eq!(
        ImmutableDataSchema::new("events", vec![ImmutableDataColumn::new("")]),
        Err(ImmutableDataError::EmptyColumnName { column: 0 })
    );
    assert_eq!(
        ImmutableDataSchema::new("events", vec![bytes.clone(), bytes]),
        Err(ImmutableDataError::DuplicateColumn {
            name: "payload".into()
        })
    );
}

#[test]
fn append_validation_requires_complete_rows_and_exact_types() {
    let row = ImmutableDataRow::new(vec![CellValue::from_u64(7), CellValue::from_bytes(b"")]);
    let schema = schema_for(row.values());
    assert_eq!(
        schema.validate_row(&ImmutableDataRow::default()),
        Err(ImmutableDataError::RowArity {
            expected: 2,
            actual: 0
        })
    );
    assert!(matches!(
        schema.validate_row(&ImmutableDataRow::new(vec![
            CellValue::from_u32(7),
            CellValue::from_bytes(b""),
        ])),
        Err(ImmutableDataError::TypeMismatch { column: 0, .. })
    ));
    // Opaque bytes is a logical type, not permission to pass any cell type.
    assert!(matches!(
        schema.validate_row(&ImmutableDataRow::new(vec![
            CellValue::from_u64(7),
            CellValue::from_str(""),
        ])),
        Err(ImmutableDataError::TypeMismatch { column: 1, .. })
    ));
    schema.validate_row(&row).unwrap();
}

#[test]
fn projection_fetches_only_selected_columns_in_caller_order() {
    let schema = schema_for(&[
        CellValue::from_u64(0),
        CellValue::from_bool(false),
        CellValue::from_str(""),
    ]);
    assert_eq!(
        schema
            .projection(&ImmutableDataReadOptions::default())
            .unwrap()
            .indices(),
        &[0, 1, 2]
    );
    let projection = schema
        .projection(&ImmutableDataReadOptions::columns(["c2", "c0"]))
        .unwrap();
    assert_eq!(projection.indices(), &[2, 0]);
    let row = {
        // Malformed unselected column must not be needed for projected decoding.
        let source = [42_u64.to_be_bytes().to_vec(), vec![255], b"hello".to_vec()];
        let selected = projection
            .indices()
            .iter()
            .map(|&i| source[i].as_slice())
            .collect::<Vec<_>>();
        projection.decode(&selected).unwrap()
    };
    // Read result owns its payloads after the reader's buffers are dropped.
    assert_eq!(row.get::<String>("c2").unwrap(), "hello");
    assert_eq!(row.get::<u64>("c0").unwrap(), 42);
    assert!(matches!(
        projection.decode(&[b"hello"]),
        Err(ImmutableDataError::PayloadArity {
            expected: 2,
            actual: 1
        })
    ));
    assert!(
        matches!(projection.decode(&[&[255], &42_u64.to_be_bytes()]),
        Err(ImmutableDataError::InvalidPayload { column: 2, name, .. }) if name == "c2")
    );
}

#[test]
fn invalid_projections_are_rejected_before_reading_payloads() {
    let schema = schema_for(&[CellValue::from_bool(false)]);
    assert!(matches!(
        schema.projection(&ImmutableDataReadOptions::columns(Vec::<String>::new())),
        Err(ImmutableDataError::EmptyProjection)
    ));
    assert!(
        matches!(schema.projection(&ImmutableDataReadOptions::columns(["missing"])),
        Err(ImmutableDataError::UnknownColumn { name }) if name == "missing")
    );
    assert!(
        matches!(schema.projection(&ImmutableDataReadOptions::columns(["c0", "c0"])),
        Err(ImmutableDataError::DuplicateProjection { name }) if name == "c0")
    );
}

#[test]
fn named_access_is_independent_of_projection_order() {
    let schema = schema_for(&[
        CellValue::from_u64(0),
        CellValue::from_u64(0),
        CellValue::from_bytes(b""),
        CellValue::from_bool(false),
    ]);
    let source = [
        10_u64.to_be_bytes().to_vec(),
        20_u64.to_be_bytes().to_vec(),
        b"body".to_vec(),
    ];
    for names in [["c0", "c1", "c2"], ["c2", "c1", "c0"], ["c1", "c0", "c2"]] {
        let projection = schema
            .projection(&ImmutableDataReadOptions::columns(names))
            .unwrap();
        let payloads = projection
            .indices()
            .iter()
            .map(|&i| source[i].as_slice())
            .collect::<Vec<_>>();
        let row = projection.decode(&payloads).unwrap();
        assert_eq!(row.get::<u64>("c0").unwrap(), 10);
        assert_eq!(row.get::<u64>("c1").unwrap(), 20);
        assert_eq!(row.get::<Vec<u8>>("c2").unwrap(), b"body");
        assert_eq!(
            row.iter()
                .unwrap()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            names
        );
        assert_eq!(
            row.value("c3"),
            Err(ImmutableDataError::ColumnNotSelected { name: "c3".into() })
        );
        assert_eq!(
            row.get::<bool>("c3"),
            Err(ImmutableDataError::ColumnNotSelected { name: "c3".into() })
        );
        assert_eq!(
            row.value("unknown"),
            Err(ImmutableDataError::UnknownColumn {
                name: "unknown".into()
            })
        );
        assert_eq!(
            row.get::<u64>("C0"),
            Err(ImmutableDataError::UnknownColumn { name: "C0".into() })
        );
        assert!(matches!(
            row.get::<u32>("c0"),
            Err(ImmutableDataError::TypeMismatch {
                expected: CellType::Uint(Width::W4),
                actual: CellType::Uint(Width::W8),
                ..
            })
        ));
    }
}

#[test]
fn read_rows_own_and_share_metadata_and_offer_borrowed_payloads() {
    let (first, second) = {
        let schema = schema_for(&[CellValue::from_str(""), CellValue::from_bytes(b"")]);
        let projection = schema
            .projection(&ImmutableDataReadOptions::columns(["c1", "c0"]))
            .unwrap();
        let bytes = vec![0, 255, 42];
        let text = String::from("hello");
        (
            projection.decode(&[&bytes, text.as_bytes()]).unwrap(),
            projection.decode(&[b"", b"other"]).unwrap(),
        )
    };
    // Original schema, projection and input buffers have all been dropped.
    let first_name = first.iter().unwrap().next().unwrap().0;
    let second_name = second.iter().unwrap().next().unwrap().0;
    assert!(std::ptr::eq(first_name, second_name));
    let borrowed = first.value("c1").unwrap().as_bytes().unwrap();
    assert_eq!(borrowed, &[0, 255, 42]);
    assert!(std::ptr::eq(
        borrowed,
        first.values()[0].as_bytes().unwrap()
    ));
    assert_eq!(first.value("c0").unwrap().as_str(), Some("hello"));
    assert_eq!(second.get::<String>("c0").unwrap(), "other");
    let mut owned = first.get::<Vec<u8>>("c1").unwrap();
    owned[0] = 99;
    assert_eq!(borrowed[0], 0);
    let cloned = first.clone();
    drop(first);
    assert_eq!(cloned.get::<String>("c0").unwrap(), "hello");
}

#[test]
fn append_rows_remain_ordered_and_report_missing_name_metadata() {
    let row = ImmutableDataRow::new(vec![CellValue::from_u64(7)]);
    assert_eq!(row.values()[0].as_u64(), Some(7));
    assert_eq!(row.get::<u64>("c0"), Err(ImmutableDataError::UnnamedRow));
    assert_eq!(row.value("c0"), Err(ImmutableDataError::UnnamedRow));
    assert!(matches!(row.iter(), Err(ImmutableDataError::UnnamedRow)));
    // Validation alone does not attach a layout or mutate the append request.
    schema_for(row.values()).validate_row(&row).unwrap();
    assert_eq!(row.value("c0"), Err(ImmutableDataError::UnnamedRow));
}

#[test]
fn named_native_conversions_cover_supported_rust_types() {
    fn check<T: FromImmutableDataValue + PartialEq + std::fmt::Debug>(
        value: CellValue,
        expected: T,
    ) {
        let schema = schema_for(std::slice::from_ref(&value));
        let row = schema.decode_row(&[value.value()]).unwrap();
        assert_eq!(row.get::<T>("c0").unwrap(), expected);
    }
    check(CellValue::from_bool(true), true);
    check(CellValue::from_u32(u32::MAX), u32::MAX);
    check(CellValue::from_u64(u64::MAX), u64::MAX);
    check(CellValue::from_u128(u128::MAX), u128::MAX);
    check(CellValue::from_i32(i32::MIN), i32::MIN);
    check(CellValue::from_i64(i64::MIN), i64::MIN);
    check(CellValue::from_i128(i128::MIN), i128::MIN);
    check(CellValue::from_f32(-1.5).unwrap(), -1.5_f32);
    check(CellValue::from_f64(f64::INFINITY).unwrap(), f64::INFINITY);
    check(CellValue::from_bytes4([4; 4]), [4_u8; 4]);
    check(CellValue::from_bytes8([8; 8]), [8_u8; 8]);
    check(CellValue::from_bytes16([16; 16]), [16_u8; 16]);
    check(CellValue::from_bytes20([20; 20]), [20_u8; 20]);
    check(CellValue::from_bytes32([32; 32]), [32_u8; 32]);
    check(CellValue::from_str(""), String::new());
    check(CellValue::from_bytes(b""), Vec::<u8>::new());
}

#[test]
fn stored_payload_validation_reuses_cell_rules() {
    for (ty, payload) in [
        (CellType::Bool, vec![2]),
        (CellType::Str, vec![255]),
        (CellType::Uint(Width::W8), vec![0; 4]),
        (CellType::FixedBytes(Width::W32), vec![0; 31]),
        // Ordered encodings of negative zero and a positive NaN.
        (
            CellType::Float(FloatWidth::F32),
            0x7fff_ffff_u32.to_be_bytes().to_vec(),
        ),
        (
            CellType::Float(FloatWidth::F64),
            0xfff8_0000_0000_0000_u64.to_be_bytes().to_vec(),
        ),
    ] {
        let schema =
            ImmutableDataSchema::new("events", vec![ImmutableDataColumn::new("v").with_type(ty)])
                .unwrap();
        assert!(matches!(
            schema.decode_row(&[&payload]),
            Err(ImmutableDataError::InvalidPayload { .. })
        ));
    }
}

#[test]
fn named_access_rejects_semantic_type_mismatches_and_supports_wrappers() {
    for value in [
        CellValue::from_timestamp64(1),
        CellValue::from_dec64_unscaled(1),
    ] {
        let schema = schema_for(std::slice::from_ref(&value));
        let row = schema.decode_row(&[value.value()]).unwrap();
        assert!(matches!(
            row.get::<i64>("c0"),
            Err(ImmutableDataError::TypeMismatch { column: 0, .. })
        ));
    }
    let value = CellValue::from_u256_be([0; 32]);
    let row = schema_for(std::slice::from_ref(&value))
        .decode_row(&[value.value()])
        .unwrap();
    assert!(matches!(
        row.get::<[u8; 32]>("c0"),
        Err(ImmutableDataError::TypeMismatch { .. })
    ));
    #[derive(Debug, PartialEq)]
    struct Days(i32);
    impl FromImmutableDataValue for Days {
        const CELL_TYPE: CellType = CellType::Date32;
        fn from_value(value: &CellValue) -> Option<Self> {
            value.as_date32().map(Self)
        }
    }
    let date = CellValue::from_date32(-7);
    let row = schema_for(std::slice::from_ref(&date))
        .decode_row(&[date.value()])
        .unwrap();
    assert_eq!(row.get::<Days>("c0").unwrap(), Days(-7));

    #[derive(Debug)]
    struct NonnegativeDays;
    impl FromImmutableDataValue for NonnegativeDays {
        const CELL_TYPE: CellType = CellType::Date32;
        fn from_value(value: &CellValue) -> Option<Self> {
            (value.as_date32()? >= 0).then_some(Self)
        }
    }
    assert!(matches!(
        row.get::<NonnegativeDays>("c0"),
        Err(ImmutableDataError::NativeConversion {
            column: 0,
            value_type: CellType::Date32
        })
    ));
}

#[test]
fn payloads_are_not_subject_to_cell_size_limits() {
    let row = ImmutableDataRow::new(vec![
        CellValue::from_bytes(&vec![255; 1024 * 1024]),
        CellValue::from_str(&"x".repeat(1024 * 1024)),
        CellValue::from_bytes(b""),
        CellValue::from_str(""),
    ]);
    let schema = schema_for(row.values());
    schema.validate_row(&row).unwrap();
    assert_eq!(
        schema
            .decode_row(&row.payloads().collect::<Vec<_>>())
            .unwrap()
            .values(),
        row.values()
    );
}

#[test]
fn addresses_preserve_the_full_ordinal_width() {
    let ordinal: ImmutableDataOrdinal = u64::MAX;
    assert_eq!(
        ImmutableDataAddress::Ordinal(ordinal),
        ImmutableDataAddress::Ordinal(u64::MAX)
    );
    assert_ne!(
        ImmutableDataAddress::Ordinal(ordinal),
        ImmutableDataAddress::Key(ImmutableDataKey([255; 32]))
    );
}

proptest! {
    #[test]
    fn arbitrary_payload_round_trip(
        number in any::<u64>(), signed in any::<i64>(), text in any::<String>(),
        bytes in prop::collection::vec(any::<u8>(), 0..1024),
    ) {
        let row = ImmutableDataRow::new(vec![
            CellValue::from_u64(number), CellValue::from_i64(signed),
            CellValue::from_str(&text), CellValue::from_bytes(&bytes),
        ]);
        let schema = schema_for(row.values());
        let decoded = schema.decode_row(&row.payloads().collect::<Vec<_>>()).unwrap();
        prop_assert_eq!(decoded.get::<u64>("c0").unwrap(), number);
        prop_assert_eq!(decoded.get::<i64>("c1").unwrap(), signed);
        prop_assert_eq!(decoded.get::<String>("c2").unwrap(), text);
        prop_assert_eq!(decoded.get::<Vec<u8>>("c3").unwrap(), bytes);
    }
}
