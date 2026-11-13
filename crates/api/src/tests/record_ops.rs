use crate::*;

const KEY: RecordKey = RecordKey([1; 32]);
const BUDGET: Budget = Budget::Limited(100);

#[test]
fn builders_preserve_invalid_and_duplicate_entries_in_input_order() {
    let input = RecordOp::create(KEY, BUDGET)
        .field("z", CellValue::from_u32(1))
        .field("", CellValue::from_bool(true))
        .attribute("payload", CellValue::from_bytes(b"x"))
        .field("z", CellValue::from_u32(2));
    let entries = input.entries();
    assert_eq!(
        entries
            .iter()
            .map(|e| e.name.as_bytes())
            .collect::<Vec<_>>(),
        [b"z".as_slice(), b"", b"payload", b"z"]
    );
    assert_eq!(entries[2].value.as_bytes(), Some(b"x".as_slice()));
    assert_eq!(entries[2].value.kind(), CellKind::Field);
    assert_eq!(entries[2].kind, CellKind::Attribute);
    assert_eq!(entries[0].value.as_u32(), Some(1));
    assert_eq!(entries[3].value.as_u32(), Some(2));
    assert_eq!(input.clone(), input);
    let expected = input.entries().to_vec();
    let (key, budget, submitted) = input.into_create();
    assert_eq!((key, budget), (KEY, BUDGET));
    assert_eq!(submitted, expected);
}

#[test]
fn patch_keeps_all_actions_and_values_without_kind_conversion() {
    let indexed = CellValue::from_i32(1)
        .with_kind(CellKind::Attribute)
        .unwrap();
    let patch = RecordOp::patch(KEY, BUDGET)
        .attribute("a", CellValue::from_bytes(b"not indexable"))
        .field("b", indexed.clone())
        .set("b", CellValue::from_u32(2))
        .remove("b")
        .remove("#key");
    assert!(matches!(&patch.entries()[0].change,
        PatchChange::Write { value, kind: CellKind::Attribute } if value.kind() == CellKind::Field));
    assert!(matches!(&patch.entries()[1].change,
        PatchChange::Write { value, kind: CellKind::Field } if value == &indexed));
    assert!(matches!(&patch.entries()[2].change, PatchChange::Set(_)));
    assert_eq!(patch.entries()[3].change, PatchChange::Remove);
    assert_eq!(patch.entries()[4].name.as_bytes(), b"#key");
    let expected = patch.entries().to_vec();
    let (key, budget, submitted) = patch.into_patch();
    assert_eq!((key, budget), (KEY, BUDGET));
    assert_eq!(submitted, expected);
}

#[test]
fn names_and_values_are_owned_without_admission_checks() {
    for name in ["", "#key", "@admin", "$", "1name", "a\0b", "a b", "é"] {
        let create = RecordOp::create(KEY, BUDGET).field(name, CellValue::from_bool(true));
        assert_eq!(create.entries()[0].name.as_bytes(), name.as_bytes());
        let patch = RecordOp::patch(KEY, BUDGET).remove(name);
        assert_eq!(patch.entries()[0].name.as_bytes(), name.as_bytes());
    }
    let name = "a".repeat(1024);
    let value = "v".repeat(2048);
    let input = RecordOp::create(KEY, BUDGET).field(&name, CellValue::from_str(&value));
    drop(name);
    drop(value);
    assert_eq!(input.entries()[0].name.as_bytes().len(), 1024);
    assert_eq!(input.entries()[0].value.as_str().unwrap().len(), 2048);
}

#[test]
fn projections_preserve_order_duplicates_and_raw_bytes() {
    let name = String::from("price");
    let read =
        RecordOp::get(KEY, Budget::Unlimited).only([name.as_bytes(), b"#key", &[0, 255], b"price"]);
    drop(name);
    assert_eq!(
        read.names()
            .unwrap()
            .iter()
            .map(CellName::as_bytes)
            .collect::<Vec<_>>(),
        [b"price".as_slice(), b"#key", &[0, 255], b"price"]
    );
    assert_eq!(read.budget(), Budget::Unlimited);
    assert_eq!(read.max_cost(), None);
    assert_eq!(RecordOp::get(KEY, BUDGET).names(), None);
    assert_eq!(read.only([] as [&str; 0]).names(), Some([].as_slice()));
    let delete = RecordOp::delete(KEY, BUDGET);
    assert_eq!(delete.record_key(), KEY);
    assert_eq!(delete.budget(), BUDGET);
    assert_eq!(delete.max_cost(), Some(100));
    assert_eq!(delete.clone(), delete);
}
