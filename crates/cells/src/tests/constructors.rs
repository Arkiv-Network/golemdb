use super::*;
use proptest::prelude::*;

#[test]
fn every_supported_type_reconstructs_its_existing_wire_vectors() {
    let mut covered = std::collections::BTreeSet::new();
    for (label, bytes, _, _, _) in VECTORS {
        let expected = CellValue::parse(bytes.to_vec()).unwrap();
        let value = match expected.cell_type() {
            CellType::Bool => CellValue::from_bool(expected.as_bool().unwrap()),
            CellType::Str => CellValue::from_str(expected.as_str().unwrap()),
            CellType::Bytes => CellValue::from_bytes(expected.as_bytes().unwrap()),
            CellType::Bytes20 => CellValue::from_bytes20(expected.as_bytes20().unwrap()),
            CellType::FixedBytes(Width::W4) => {
                CellValue::from_bytes4(expected.as_bytes4().unwrap())
            }
            CellType::FixedBytes(Width::W8) => {
                CellValue::from_bytes8(expected.as_bytes8().unwrap())
            }
            CellType::FixedBytes(Width::W16) => {
                CellValue::from_bytes16(expected.as_bytes16().unwrap())
            }
            CellType::FixedBytes(Width::W32) => {
                CellValue::from_bytes32(expected.as_bytes32().unwrap())
            }
            CellType::Uint(Width::W4) => CellValue::from_u32(expected.as_u32().unwrap()),
            CellType::Uint(Width::W8) => CellValue::from_u64(expected.as_u64().unwrap()),
            CellType::Uint(Width::W16) => CellValue::from_u128(expected.as_u128().unwrap()),
            CellType::Uint(Width::W32) => CellValue::from_u256_be(expected.as_u256_be().unwrap()),
            CellType::Int(Width::W4) => CellValue::from_i32(expected.as_i32().unwrap()),
            CellType::Int(Width::W8) => CellValue::from_i64(expected.as_i64().unwrap()),
            CellType::Int(Width::W16) => CellValue::from_i128(expected.as_i128().unwrap()),
            CellType::Int(Width::W32) => CellValue::from_i256_be(expected.as_i256_be().unwrap()),
            CellType::Decimal(Width::W4) => {
                CellValue::from_dec32_unscaled(expected.as_dec32_unscaled().unwrap())
            }
            CellType::Decimal(Width::W8) => {
                CellValue::from_dec64_unscaled(expected.as_dec64_unscaled().unwrap())
            }
            CellType::Decimal(Width::W16) => {
                CellValue::from_dec128_unscaled(expected.as_dec128_unscaled().unwrap())
            }
            CellType::Decimal(Width::W32) => {
                CellValue::from_dec256_unscaled_be(expected.as_dec256_unscaled_be().unwrap())
            }
            CellType::Float(FloatWidth::F32) => {
                CellValue::from_f32(expected.as_f32().unwrap()).unwrap()
            }
            CellType::Float(FloatWidth::F64) => {
                CellValue::from_f64(expected.as_f64().unwrap()).unwrap()
            }
            CellType::Date32 => CellValue::from_date32(expected.as_date32().unwrap()),
            CellType::Timestamp64 => {
                CellValue::from_timestamp64(expected.as_timestamp64().unwrap())
            }
        };
        covered.insert(value.cell_type().id());
        assert_eq!(value.kind(), CellKind::Field, "{label}");
        assert_eq!(value.as_view().kind(), CellKind::Field, "{label}");
        assert!(!value.is_indexable());
        assert_eq!(
            value.with_kind(expected.kind()).unwrap().encoded_bytes(),
            *bytes,
            "{label}"
        );
    }
    for id in 0..128 {
        assert_eq!(
            covered.contains(&id),
            CellType::from_id(id).is_ok(),
            "type {id}"
        );
    }
}

#[test]
fn signed_values_are_encoded_without_caller_transformations() {
    assert_eq!(
        CellValue::from_i32(50).encoded_bytes(),
        &[0x10, 0x80, 0, 0, 0x32]
    );
    assert_eq!(CellValue::from_i32(i32::MIN).value(), &[0; 4]);
    assert_eq!(CellValue::from_i32(i32::MAX).value(), &[255; 4]);
    assert_eq!(CellValue::from_i64(i64::MIN).value(), &[0; 8]);
    assert_eq!(CellValue::from_i64(i64::MAX).value(), &[255; 8]);
    assert_eq!(CellValue::from_i128(i128::MIN).value(), &[0; 16]);
    assert_eq!(CellValue::from_i128(i128::MAX).value(), &[255; 16]);
    let mut minimum = [0; 32];
    minimum[0] = 0x80;
    let mut maximum = [255; 32];
    maximum[0] = 0x7f;
    assert_eq!(CellValue::from_i256_be(minimum).value(), &[0; 32]);
    assert_eq!(CellValue::from_i256_be(maximum).value(), &[255; 32]);
    assert_eq!(
        CellValue::from_dec256_unscaled_be(minimum).value(),
        &[0; 32]
    );
    assert_eq!(
        CellValue::from_dec256_unscaled_be(maximum).value(),
        &[255; 32]
    );
    assert_eq!(CellValue::from_date32(i32::MIN).value(), &[0; 4]);
    assert_eq!(CellValue::from_timestamp64(i64::MIN).value(), &[0; 8]);
}

#[test]
fn kind_conversion_only_changes_metadata_and_rejects_unindexable_bytes() {
    let field = CellValue::from_i32(-50);
    let attribute = field.clone().with_kind(CellKind::Attribute).unwrap();
    assert_eq!(attribute.kind(), CellKind::Attribute);
    assert_eq!(attribute.as_view().kind(), CellKind::Attribute);
    assert_eq!(attribute.metadata(), field.metadata() | 0x80);
    assert_eq!(attribute.value(), field.value());
    assert_eq!(attribute.cell_type(), field.cell_type());
    assert_eq!(
        attribute.clone().with_kind(CellKind::Attribute).unwrap(),
        attribute
    );
    assert_eq!(attribute.with_kind(CellKind::Field).unwrap(), field);
    for bytes in [b"".as_slice(), b"abc"] {
        assert_eq!(
            CellValue::from_bytes(bytes).with_kind(CellKind::Attribute),
            Err(CellValueParseError::NotIndexable)
        );
        assert_eq!(
            CellValue::from_bytes(bytes)
                .with_kind(CellKind::Field)
                .unwrap()
                .as_bytes(),
            Some(bytes)
        );
    }
    assert!(
        CellValue::from_bytes32([0; 32])
            .with_kind(CellKind::Attribute)
            .is_ok()
    );
}

#[test]
fn float_construction_normalizes_zero_and_rejects_nan_but_parsing_stays_strict() {
    assert_eq!(
        CellValue::from_f32(-0.0).unwrap(),
        CellValue::from_f32(0.0).unwrap()
    );
    assert_eq!(
        CellValue::from_f64(-0.0).unwrap(),
        CellValue::from_f64(0.0).unwrap()
    );
    for bits in [0x7fc00000, 0xffc00000, 0x7f800001] {
        assert_eq!(
            CellValue::from_f32(f32::from_bits(bits)),
            Err(CellValueParseError::FloatNaN)
        );
    }
    for bits in [0x7ff8000000000000, 0xfff8000000000000, 0x7ff0000000000001] {
        assert_eq!(
            CellValue::from_f64(f64::from_bits(bits)),
            Err(CellValueParseError::FloatNaN)
        );
    }
    // Negative zero's noncanonical ordered payload must never be normalized on read.
    assert_eq!(
        CellValue::parse(vec![0x18, 0x7f, 0xff, 0xff, 0xff]),
        Err(CellValueParseError::NegativeZero)
    );
    let mut bytes = vec![0x19, 0x7f];
    bytes.extend([0xff; 7]);
    assert_eq!(
        CellValue::parse(bytes),
        Err(CellValueParseError::NegativeZero)
    );

    let mut previous: Option<CellValue> = None;
    for value in [
        f64::NEG_INFINITY,
        -f64::MAX,
        -1.0,
        -f64::MIN_POSITIVE,
        0.0,
        f64::MIN_POSITIVE,
        1.0,
        f64::MAX,
        f64::INFINITY,
    ] {
        let cell = CellValue::from_f64(value).unwrap();
        assert_eq!(cell.as_f64(), Some(value));
        if let Some(previous) = previous {
            assert!(previous.value() < cell.value());
        }
        previous = Some(cell);
    }
}

proptest! {
    #[test]
    fn typed_values_round_trip_and_preserve_numeric_order(a: i128, b: i128, n: u128, wide: [u8; 32]) {
        let left = CellValue::from_i128(a);
        let right = CellValue::from_i128(b);
        prop_assert_eq!(left.value().cmp(right.value()), a.cmp(&b));
        prop_assert_eq!(CellValue::parse(left.into_bytes()).unwrap().as_i128(), Some(a));
        prop_assert_eq!(CellValue::from_u128(n).as_u128(), Some(n));
        prop_assert_eq!(CellValue::from_dec128_unscaled(a).as_dec128_unscaled(), Some(a));
        prop_assert_eq!(CellValue::from_u256_be(wide).as_u256_be(), Some(wide));
        prop_assert_eq!(CellValue::from_i256_be(wide).as_i256_be(), Some(wide));
        prop_assert_eq!(CellValue::from_dec256_unscaled_be(wide).as_dec256_unscaled_be(), Some(wide));
    }

    #[test]
    fn all_float_bit_patterns_construct_or_reject_by_value(bits32: u32, bits64: u64) {
        let f32 = f32::from_bits(bits32);
        let f64 = f64::from_bits(bits64);
        match CellValue::from_f32(f32) {
            Ok(value) => {
                prop_assert!(!f32.is_nan());
                let decoded = CellValue::parse(value.into_bytes()).unwrap().as_f32().unwrap();
                prop_assert_eq!(decoded.to_bits(), if f32 == 0.0 { 0 } else { bits32 });
            }
            Err(error) => { prop_assert!(f32.is_nan()); prop_assert_eq!(error, CellValueParseError::FloatNaN); }
        }
        match CellValue::from_f64(f64) {
            Ok(value) => {
                prop_assert!(!f64.is_nan());
                let decoded = CellValue::parse(value.into_bytes()).unwrap().as_f64().unwrap();
                prop_assert_eq!(decoded.to_bits(), if f64 == 0.0 { 0 } else { bits64 });
            }
            Err(error) => { prop_assert!(f64.is_nan()); prop_assert_eq!(error, CellValueParseError::FloatNaN); }
        }
    }
}
