//! Unit tests: [`vectors`] holds the accept/reject tables, [`properties`] the
//! generated-input checks, [`order`] stored-form ordering, [`keys`] cell names.

mod constructors;
mod foundation;
mod keys;
mod order;
mod properties;
mod vectors;

use crate::*;
use vectors::*;

#[test]
fn vectors_decode_and_re_encode() {
    for (name, bytes, indexable, ty, value) in VECTORS {
        let cell = CellValueRef::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(cell.cell_type(), *ty, "{name}");
        assert_eq!(cell.value(), *value, "{name}");
        assert_eq!(cell.is_indexable(), *indexable, "{name}");
        assert_eq!(cell.encode(), *bytes, "{name}");

        let owned = CellValue::parse(bytes.to_vec()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(owned.cell_type(), *ty, "{name}");
        assert_eq!(owned.value(), *value, "{name}");
        assert_eq!(owned.is_indexable(), *indexable, "{name}");
        assert_eq!(owned.into_bytes(), *bytes, "{name}");
    }
}

#[test]
fn vectors_cover_every_type() {
    for id in 0..128 {
        if CellType::from_id(id).is_ok() {
            assert!(
                VECTORS.iter().any(|(_, b, ..)| b[0] & TYPE_MASK == id),
                "type id {id} has no vector"
            );
        }
    }
}

#[test]
fn bad_vectors_are_rejected_for_the_stated_reason() {
    for (name, bytes, expected) in BAD_VECTORS {
        assert_eq!(CellValueRef::parse(bytes).err(), Some(*expected), "{name}");
        assert_eq!(
            CellValue::parse(bytes.to_vec()).err(),
            Some(*expected),
            "{name}"
        );
    }
}

#[test]
fn codec_should_support_larger_than_u16_max() {
    let limits = CellLimits {
        max_cell_name_len: 1,
        max_str_len: LARGE_PAYLOAD_LEN as u32,
        max_bytes_len: LARGE_PAYLOAD_LEN as u32,
    };
    for (ty, wire, payload) in large_value_vectors() {
        let cell = CellValueRef::parse(&wire).unwrap();
        assert_eq!(cell.cell_type(), ty);
        assert_eq!(cell.value(), payload);
        assert!(!cell.is_indexable());
        assert_eq!(cell.encode(), wire);
        assert_eq!(
            CellValueRef::new(ty, &payload, false).unwrap().encode(),
            wire
        );
        assert!(limits.validate_value(cell).is_ok());
    }
}

#[test]
fn float_input_vectors_encode_and_validate() {
    fn check<const N: usize>(ty: CellType, vectors: &[FloatInputVector<N>]) {
        for (name, native, expected, validation) in vectors {
            let encoded = encode_float(*native);
            assert_eq!(encoded, *expected, "{name}");
            assert_eq!(
                CellValueRef::new(ty, &encoded, false).map(|_| ()),
                *validation,
                "{name}"
            );
        }
    }
    check(CellType::Float(FloatWidth::F32), FLOAT32_INPUTS);
    check(CellType::Float(FloatWidth::F64), FLOAT64_INPUTS);
}

#[test]
fn id_space_matches_the_spec() {
    for id in 0..128 {
        match (id, CellType::from_id(id)) {
            (0, got) => assert_eq!(got, Err(CellValueParseError::AbsentTag)),
            (5..=7 | 26 | 27 | 30.., got) => {
                assert_eq!(got, Err(CellValueParseError::ReservedType(id)))
            }
            (_, got) => assert_eq!(got.map(CellType::id), Ok(id), "id {id}"),
        }
    }
}

/// Every metadata byte against every payload length: parsing never panics,
/// and whatever parses re-encodes to the same bytes.
#[test]
fn every_metadata_byte_and_length() {
    // Payload bytes are content only; the slice supplies the boundary.
    let mut payload = vec![0x00, 0x00, 0x00, 0x01];
    payload.extend([0x01; 36]);
    for metadata in 0..=u8::MAX {
        for len in 0..=payload.len() {
            let bytes = [&[metadata][..], &payload[..len]].concat();
            let accepted = match CellType::from_id(metadata & TYPE_MASK) {
                Err(_) => false,
                Ok(CellType::Bytes) if metadata & INDEXABLE_BIT != 0 => false,
                Ok(ty) => match ty.width() {
                    Some(n) => len == n && ty.validate(&payload[..n]).is_ok(),
                    None => true,
                },
            };
            match CellValueRef::parse(&bytes) {
                Ok(cell) => {
                    assert!(accepted, "{metadata:#04x} len {len}: should fail");
                    assert_eq!(cell.encode(), bytes, "{metadata:#04x} len {len}");
                }
                Err(_) => assert!(!accepted, "{metadata:#04x} len {len}: should parse"),
            }
        }
    }
}

/// `0x00` is not a type, so a tombstone is `Option<CellValueRef>::None`, which
/// costs no space.
#[test]
fn absence_is_a_free_option() {
    assert_eq!(
        size_of::<Option<CellValueRef<'_>>>(),
        size_of::<CellValueRef<'_>>()
    );
}

#[test]
fn accessors_decode_their_rust_equivalents() {
    fn parse(bytes: &[u8]) -> CellValue {
        CellValue::parse(bytes.to_vec()).unwrap()
    }
    fn cell(id: u8, value: &[u8]) -> Vec<u8> {
        [&[id][..], value].concat()
    }

    assert_eq!(parse(&[0x01, 1]).as_bool(), Some(true));
    assert_eq!(parse(&[0x02, b'h', b'i']).as_str(), Some("hi"));
    assert_eq!(
        parse(&[0x03, 0xDE, 0xAD]).as_bytes(),
        Some(&[0xDE, 0xAD][..])
    );
    assert_eq!(parse(&[0x08, 1, 2, 3, 4]).as_bytes4(), Some([1, 2, 3, 4]));
    assert_eq!(parse(&cell(0x04, &[7; 20])).as_bytes20(), Some([7; 20]));
    assert_eq!(
        parse(&cell(0x0D, &u64::MAX.to_be_bytes())).as_u64(),
        Some(u64::MAX)
    );

    // Signed values and floats are stored in order form; the accessors decode.
    assert_eq!(parse(&[0x10, 0x7F, 0xFF, 0xFF, 0xFF]).as_i32(), Some(-1));
    assert_eq!(
        parse(&cell(0x11, &flip_sign(i64::MIN.to_be_bytes()))).as_i64(),
        Some(i64::MIN)
    );
    assert_eq!(
        parse(&cell(0x13, &flip_sign([0xFF; 32]))).as_i256_be(),
        Some([0xFF; 32])
    );
    assert_eq!(
        parse(&cell(0x14, &flip_sign((-5i32).to_be_bytes()))).as_dec32_unscaled(),
        Some(-5)
    );
    assert_eq!(parse(&[0x18, 0xBF, 0xC0, 0, 0]).as_f32(), Some(1.5));
    assert_eq!(
        parse(&cell(0x19, &encode_float((-0.25f64).to_be_bytes()))).as_f64(),
        Some(-0.25)
    );
    assert_eq!(
        parse(&cell(0x1C, &flip_sign(20_000i32.to_be_bytes()))).as_date32(),
        Some(20_000)
    );
    assert_eq!(
        parse(&cell(0x1D, &flip_sign((-1i64).to_be_bytes()))).as_timestamp64(),
        Some(-1)
    );
}

/// The same eight bytes under `bytes8`, `u64`, `i64`, `dec64`, `f64` and
/// `timestamp64` read out under exactly one accessor each.
#[test]
fn accessors_are_exclusive() {
    let payload = [0x80, 0, 0, 0, 0, 0, 0, 1];
    for id in [0x09, 0x0D, 0x11, 0x15, 0x19, 0x1D] {
        let bytes = [&[id][..], &payload].concat();
        let cell = CellValue::parse(bytes).unwrap();
        let hits = [
            cell.as_bytes8().is_some(),
            cell.as_u64().is_some(),
            cell.as_i64().is_some(),
            cell.as_dec64_unscaled().is_some(),
            cell.as_f64().is_some(),
            cell.as_timestamp64().is_some(),
        ];
        assert_eq!(hits.iter().filter(|h| **h).count(), 1, "id {id:#04x}");
        assert_eq!(cell.as_u32(), None, "id {id:#04x}");
    }
}

/// `bytes` accepts what `str` rejects: UTF-8 is the type's rule, not the
/// format's.
#[test]
fn bytes_accepts_what_str_rejects() {
    for (name, bytes, expected) in BAD_VECTORS {
        if let CellValueParseError::InvalidUtf8 { .. } = expected {
            let as_bytes = [&[CellType::Bytes.id()][..], &bytes[1..]].concat();
            let cell = CellValueRef::parse(&as_bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(cell.cell_type(), CellType::Bytes, "{name}");
        }
    }
}

mod storage;
mod storage_work;
