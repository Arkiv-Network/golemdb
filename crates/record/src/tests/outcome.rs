use golemdb_cells::{CellKind, CellNameRef};

use super::*;

fn name(bytes: &[u8]) -> CellName {
    CellNameRef::raw(bytes).into()
}

#[test]
fn encoded_sizes_include_keys_tags_and_payload_bytes() {
    // Expected sizes are independent of the encoding helpers under test.
    // Cells include an eight-byte record ID and a tag; index terms include
    // a separator and a tag, but no record ID.
    for (label, value, cell_bytes, index_bytes) in [
        ("name", CellValue::from_bool(true), 14, Some(7)),
        ("age", CellValue::from_u32(42), 16, Some(9)),
        ("counter", CellValue::from_u64(42), 24, Some(17)),
        ("n", CellValue::from_i128(-1), 26, Some(19)),
        ("tag", CellValue::from_str("é"), 14, Some(7)),
        ("x", CellValue::from_str(""), 10, Some(3)),
        ("hash", CellValue::from_bytes32([0; 32]), 45, Some(38)),
        ("blob", CellValue::from_bytes(&[0; 23]), 36, None),
    ] {
        let name = name(label.as_bytes());
        assert_eq!(cell_size(&name, &value).unwrap(), cell_bytes, "{label}");
        if let Some(index_bytes) = index_bytes {
            let attribute = value.clone().with_kind(CellKind::Attribute).unwrap();
            assert_eq!(cell_size(&name, &attribute).unwrap(), cell_bytes, "{label}");
            assert_eq!(
                index_size(&name, &attribute).unwrap(),
                index_bytes,
                "{label}"
            );
        }
    }
}

#[test]
fn changes_account_for_cells_and_index_membership() {
    let red = CellValue::from_str("red");
    let blue = CellValue::from_str("blue");
    let red_attr = red.clone().with_kind(CellKind::Attribute).unwrap();
    let blue_attr = blue.clone().with_kind(CellKind::Attribute).unwrap();
    for (label, before, after, expected) in [
        (
            "create field",
            None,
            Some(&red),
            Effects {
                cells_created: 1,
                cell_bytes_written: 16,
                ..Effects::default()
            },
        ),
        (
            "create attribute",
            None,
            Some(&red_attr),
            Effects {
                cells_created: 1,
                cell_bytes_written: 16,
                index_joins: 1,
                index_bytes_written: 9,
                ..Effects::default()
            },
        ),
        (
            "update field",
            Some(&red),
            Some(&blue),
            Effects {
                cells_updated: 1,
                cell_bytes_written: 17,
                cell_bytes_deleted: 16,
                ..Effects::default()
            },
        ),
        (
            "update attribute",
            Some(&red_attr),
            Some(&blue_attr),
            Effects {
                cells_updated: 1,
                cell_bytes_written: 17,
                cell_bytes_deleted: 16,
                index_joins: 1,
                index_leaves: 1,
                index_bytes_written: 10,
                index_bytes_deleted: 9,
                ..Effects::default()
            },
        ),
        (
            "field to attribute",
            Some(&red),
            Some(&red_attr),
            Effects {
                cells_updated: 1,
                cell_bytes_written: 16,
                cell_bytes_deleted: 16,
                index_joins: 1,
                index_bytes_written: 9,
                ..Effects::default()
            },
        ),
        (
            "attribute to field",
            Some(&red_attr),
            Some(&red),
            Effects {
                cells_updated: 1,
                cell_bytes_written: 16,
                cell_bytes_deleted: 16,
                index_leaves: 1,
                index_bytes_deleted: 9,
                ..Effects::default()
            },
        ),
        (
            "delete field",
            Some(&red),
            None,
            Effects {
                cells_deleted: 1,
                cell_bytes_deleted: 16,
                ..Effects::default()
            },
        ),
        (
            "delete attribute",
            Some(&red_attr),
            None,
            Effects {
                cells_deleted: 1,
                cell_bytes_deleted: 16,
                index_leaves: 1,
                index_bytes_deleted: 9,
                ..Effects::default()
            },
        ),
    ] {
        let mut effects = Effects::default();
        effects.change(&name(b"name"), before, after).unwrap();
        assert_eq!(effects, expected, "{label}");
    }
}

#[test]
fn unchanged_values_and_system_cells_do_not_contribute_effects() {
    let value = CellValue::from_str("red")
        .with_kind(CellKind::Attribute)
        .unwrap();
    let other = CellValue::from_str("blue");
    let initial = Effects {
        cells_created: 3,
        ..Effects::default()
    };
    let mut effects = initial;
    effects.change(&name(b"name"), None, None).unwrap();
    effects
        .change(&name(b"name"), Some(&value), Some(&value.clone()))
        .unwrap();
    for reserved in [b"#key".as_slice(), b"@weight"] {
        effects.change(&name(reserved), None, Some(&value)).unwrap();
        effects
            .change(&name(reserved), Some(&value), Some(&other))
            .unwrap();
        effects.change(&name(reserved), Some(&value), None).unwrap();
    }
    assert_eq!(effects, initial);
}

#[test]
fn effects_accumulate_across_changes() {
    let red = CellValue::from_str("red")
        .with_kind(CellKind::Attribute)
        .unwrap();
    let blue = CellValue::from_str("blue");
    let name = name(b"name");
    let mut effects = Effects::default();
    effects.change(&name, None, Some(&red)).unwrap();
    effects.change(&name, Some(&red), Some(&blue)).unwrap();
    effects.change(&name, Some(&blue), None).unwrap();
    assert_eq!(
        effects,
        Effects {
            cells_created: 1,
            cells_updated: 1,
            cells_deleted: 1,
            index_joins: 1,
            index_leaves: 1,
            cell_bytes_written: 33,
            cell_bytes_deleted: 33,
            index_bytes_written: 9,
            index_bytes_deleted: 9,
        }
    );
}

#[test]
fn every_effect_counter_rejects_overflow() {
    let before = CellValue::from_str("red")
        .with_kind(CellKind::Attribute)
        .unwrap();
    let after = CellValue::from_str("blue")
        .with_kind(CellKind::Attribute)
        .unwrap();
    for (label, mut effects, old, new) in [
        (
            "created",
            Effects {
                cells_created: u64::MAX,
                ..Effects::default()
            },
            None,
            Some(&after),
        ),
        (
            "deleted",
            Effects {
                cells_deleted: u64::MAX,
                ..Effects::default()
            },
            Some(&before),
            None,
        ),
        (
            "updated",
            Effects {
                cells_updated: u64::MAX,
                ..Effects::default()
            },
            Some(&before),
            Some(&after),
        ),
        (
            "joins",
            Effects {
                index_joins: u64::MAX,
                ..Effects::default()
            },
            Some(&before),
            Some(&after),
        ),
        (
            "leaves",
            Effects {
                index_leaves: u64::MAX,
                ..Effects::default()
            },
            Some(&before),
            Some(&after),
        ),
        (
            "cell bytes written",
            Effects {
                cell_bytes_written: u64::MAX,
                ..Effects::default()
            },
            Some(&before),
            Some(&after),
        ),
        (
            "cell bytes deleted",
            Effects {
                cell_bytes_deleted: u64::MAX,
                ..Effects::default()
            },
            Some(&before),
            Some(&after),
        ),
        (
            "index bytes written",
            Effects {
                index_bytes_written: u64::MAX,
                ..Effects::default()
            },
            Some(&before),
            Some(&after),
        ),
        (
            "index bytes deleted",
            Effects {
                index_bytes_deleted: u64::MAX,
                ..Effects::default()
            },
            Some(&before),
            Some(&after),
        ),
    ] {
        assert!(
            matches!(
                effects.change(&name(b"name"), old, new),
                Err(RecordError::AccountingOverflow)
            ),
            "{label}"
        );
    }
}

#[test]
fn checked_size_and_addition_do_not_wrap() {
    assert_eq!(size(&[]).unwrap(), 0);
    assert_eq!(size(&[8, 4, 1, 3]).unwrap(), 16);
    if usize::BITS == 64 {
        assert!(matches!(
            size(&[usize::MAX, 1]),
            Err(RecordError::AccountingOverflow)
        ));
    }
    let mut total = u64::MAX - 1;
    add(&mut total, 1).unwrap();
    add(&mut total, 0).unwrap();
    assert_eq!(total, u64::MAX);
    assert!(matches!(
        add(&mut total, 1),
        Err(RecordError::AccountingOverflow)
    ));
    assert_eq!(total, u64::MAX);
}

#[test]
fn outcomes_keep_context_but_discard_effects_on_failure() {
    let head = Some(CommitId::new(7));
    let effects = Effects {
        cells_created: 1,
        cell_bytes_written: 16,
        ..Effects::default()
    };
    let success = Outcome::<_, RecordError>::new(Ok(42), head, effects);
    assert_eq!(success.result.unwrap(), 42);
    assert_eq!(success.priced_at, head);
    assert_eq!(success.effects, effects);
    for head in [head, None] {
        let failed = Outcome::<(), _>::new(Err(RecordError::AccountingOverflow), head, effects);
        assert!(matches!(
            failed.result,
            Err(RecordError::AccountingOverflow)
        ));
        assert_eq!(failed.priced_at, head);
        assert_eq!(failed.effects, Effects::default());
    }
}
