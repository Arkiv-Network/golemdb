use proptest::prelude::*;

use crate::*;

#[test]
fn cell_batch_keeps_last_operation_and_reports_changes_in_key_order() {
    use golemdb_merkle::{Keccak256Hasher, RootRef};
    use golemdb_storage::{MemoryStore, Store};

    let db = MemoryStore::new();
    let cells = Cells::new(&Keccak256Hasher);
    let mut tx = db.begin_write().unwrap();
    let key = |id| CellKey::new(id, CellNameRef::raw(b"value"));
    let value = |byte| CellValue::parse(vec![0x01, byte]).unwrap();
    let put = |id, byte| CellChange::Put {
        key: key(id),
        value: value(byte),
    };
    let delete = |id| CellChange::Delete { key: key(id) };
    let initial = cells
        .apply(&mut tx, RootRef::Empty, [put(2, 0), put(3, 1)])
        .unwrap();
    let update = cells
        .apply(
            &mut tx,
            initial.root,
            [
                put(3, 0),
                put(1, 1),
                delete(2),
                put(4, 1),
                delete(1),
                put(2, 1),
                delete(3),
                delete(4),
                put(1, 0),
                put(2, 0),
                delete(3),
            ],
        )
        .unwrap();

    assert_eq!(
        update.changed_cells,
        vec![
            CellValueChange {
                key: key(1),
                before: None,
                after: Some(value(0))
            },
            CellValueChange {
                key: key(3),
                before: Some(value(1)),
                after: None
            },
        ]
    );
    for (id, expected) in [
        (1, Some(value(0))),
        (2, Some(value(0))),
        (3, None),
        (4, None),
    ] {
        assert_eq!(cells.get(&tx, &key(id)).unwrap(), expected);
    }
    let fresh_db = MemoryStore::new();
    let mut fresh_tx = fresh_db.begin_write().unwrap();
    let fresh = cells
        .apply(&mut fresh_tx, RootRef::Empty, [put(1, 0), put(2, 0)])
        .unwrap();
    assert_eq!(update.root, fresh.root);
}

#[test]
fn configured_cell_limits_are_enforced() {
    let limits = CellLimits {
        max_cell_name_len: 6,
        max_str_len: 2,
        max_bytes_len: 4,
    };
    assert!(limits.parse_user_name(b"$owner").is_ok());
    assert_eq!(
        limits.parse_user_name(b"$owners"),
        Err(CellNameError::TooLong { max: 6, actual: 7 })
    );
    for (ty, max) in [(CellType::Str, 2), (CellType::Bytes, 4)] {
        for len in [0, max, max + 1] {
            let value = vec![b'a'; len];
            let cell = CellValueRef::new(ty, &value, false).unwrap();
            let expected = if len <= max {
                Ok(())
            } else {
                Err(CellLimitError {
                    ty,
                    max: max as u32,
                    actual: len,
                })
            };
            assert_eq!(limits.validate_value(cell), expected);
        }
    }
    assert!(
        limits
            .validate_value(CellValueRef::new(CellType::Str, "é".as_bytes(), true).unwrap())
            .is_ok()
    );
    assert!(
        limits
            .validate_value(CellValueRef::new(CellType::Str, "éa".as_bytes(), false).unwrap())
            .is_err()
    );
    let zero = CellLimits {
        max_cell_name_len: 0,
        max_str_len: 0,
        max_bytes_len: 0,
    };
    assert!(zero.parse_user_name(b"a").is_err());
    assert!(
        zero.validate_value(CellValueRef::parse(&[2]).unwrap())
            .is_ok()
    );
    assert!(
        zero.validate_value(CellValueRef::parse(&[3]).unwrap())
            .is_ok()
    );
    assert!(
        zero.validate_value(CellValueRef::parse(&[1, 1]).unwrap())
            .is_ok()
    );
}

#[test]
fn full_keys_encode_record_id_then_raw_name() {
    let key = CellKey::new(42, CellNameRef::parse_user(b"Price", 64).unwrap());
    assert_eq!(key.encode(), b"\0\0\0\0\0\0\0\x2aPrice");
    assert_eq!(key.record_id(), 42);
    assert_eq!(key.name().as_bytes(), b"Price");
    assert_eq!(CellKey::decode(&key.encode()).unwrap(), key);
    for name in [&b""[..], b"#key", b"@weight", b"\0\xff\x80"] {
        let key = CellKey::new(u64::MAX, CellNameRef::raw(name));
        assert_eq!(CellKey::decode(&key.encode()).unwrap(), key);
    }
    for len in 0..8 {
        assert_eq!(
            CellKey::decode(&[0; 8][..len]),
            Err(CellKeyError::MissingRecordId { actual: len })
        );
    }
}

proptest! {
    #[test]
    fn key_order_matches_storage_bytes(
        a in any::<u64>(), b in any::<u64>(),
        x in prop::collection::vec(any::<u8>(), 0..80),
        y in prop::collection::vec(any::<u8>(), 0..80),
    ) {
        let a = CellKey::new(a, CellNameRef::raw(&x));
        let b = CellKey::new(b, CellNameRef::raw(&y));
        prop_assert_eq!(a.cmp(&b), a.encode().cmp(&b.encode()));
        prop_assert_eq!(CellKey::decode(&a.encode()).unwrap(), a);
    }
}
