use crate::*;

const KEY: RecordKey = RecordKey([1; 32]);

#[test]
fn record_kind_helpers_encode_explicit_types_and_insert_preserves_kind() {
    let indexed = CellValue::from_str("kept")
        .with_kind(CellKind::Attribute)
        .unwrap();
    let input = RecordOp::create(KEY)
        .attribute("price", CellValue::from_i32(50))
        .unwrap()
        .field("description", indexed.clone())
        .unwrap()
        .insert("tag", indexed.clone())
        .unwrap()
        .field("payload", CellValue::from_bytes(&[0, 255]))
        .unwrap();
    let cells = input;
    assert_eq!(cells.record_key(), KEY);
    assert_eq!(cells.cells().len(), 4);
    assert_eq!(cells.clone(), cells);
    assert_eq!(
        cells.value("price").unwrap().encoded_bytes(),
        &[0x90, 0x80, 0, 0, 50]
    );
    assert_eq!(cells.value("description").unwrap().kind(), CellKind::Field);
    assert_eq!(cells.value("tag").unwrap(), &indexed);
    assert_eq!(
        cells.value("payload").unwrap().as_bytes(),
        Some([0, 255].as_slice())
    );
    assert!(matches!(
        RecordOp::create(KEY).attribute("payload", CellValue::from_bytes(b"")),
        Err(ApiError::InvalidArgument { .. })
    ));
}

#[test]
fn patch_supports_kind_overrides_preserved_sets_and_removals() {
    let indexed = CellValue::from_i32(1)
        .with_kind(CellKind::Attribute)
        .unwrap();
    let patch = RecordOp::patch(KEY)
        .attribute("price", CellValue::from_i32(75))
        .unwrap()
        .field("plain", indexed.clone())
        .unwrap()
        .set("preserve", indexed.clone())
        .unwrap()
        .remove("description")
        .unwrap();
    assert_eq!(patch.record_key(), KEY);
    assert_eq!(patch.clone(), patch);
    assert_eq!(patch.value("price").unwrap().as_i32(), Some(75));
    assert_eq!(patch.value("price").unwrap().kind(), CellKind::Attribute);
    assert_eq!(patch.value("plain").unwrap().kind(), CellKind::Field);
    assert_eq!(patch.value("preserve"), Some(&indexed));
    assert!(patch.removes("description"));
    assert!(!patch.removes("price"));
    assert!(!patch.removes("missing"));
    assert!(patch.value("description").is_none());
    assert!(patch.value("missing").is_none());
    assert_eq!(patch.changes().len(), 4);
    let changes: Vec<_> = patch
        .changes()
        .map(|(name, value)| (name.as_bytes(), value))
        .collect();
    assert_eq!(changes[0], (b"description".as_slice(), None));
    assert_eq!(changes[1], (b"plain".as_slice(), patch.value("plain")));
    assert!(matches!(
        RecordOp::patch(KEY).attribute("payload", CellValue::from_bytes(b"")),
        Err(ApiError::InvalidArgument { .. })
    ));
}

#[test]
fn duplicate_names_fail_across_helper_kinds_and_patch_operations() {
    let input = RecordOp::create(KEY)
        .field("price", CellValue::from_i32(1))
        .unwrap();
    assert!(matches!(
        input.clone().insert("price", CellValue::from_i32(2)),
        Err(ApiError::InvalidArgument { .. })
    ));
    assert!(matches!(
        input.clone().attribute("price", CellValue::from_i32(2)),
        Err(ApiError::InvalidArgument { .. })
    ));
    assert!(matches!(
        input.field("price", CellValue::from_i32(2)),
        Err(ApiError::InvalidArgument { .. })
    ));
    for patch in [
        RecordOp::patch(KEY)
            .set("price", CellValue::from_i32(1))
            .unwrap(),
        RecordOp::patch(KEY).remove("price").unwrap(),
    ] {
        assert!(matches!(
            patch.clone().remove("price"),
            Err(ApiError::InvalidArgument { .. })
        ));
        assert!(matches!(
            patch.set("price", CellValue::from_i32(2)),
            Err(ApiError::InvalidArgument { .. })
        ));
    }
    assert!(
        RecordOp::create(KEY)
            .field("Price", CellValue::from_i32(1))
            .unwrap()
            .field("price", CellValue::from_i32(2))
            .is_ok()
    );
}

#[test]
fn builders_validate_grammar_but_leave_deployment_limits_to_crud() {
    for invalid in ["", "#key", "@admin", "$", "1name", "a\0b", "a b", "é"] {
        assert!(matches!(
            RecordOp::create(KEY).field(invalid, CellValue::from_bool(true)),
            Err(ApiError::InvalidArgument { .. })
        ));
        assert!(matches!(
            RecordOp::patch(KEY).remove(invalid),
            Err(ApiError::InvalidArgument { .. })
        ));
    }
    assert!(
        RecordOp::create(KEY)
            .field("$owner", CellValue::from_u64(1))
            .is_ok()
    );
    assert!(
        RecordOp::create(KEY)
            .field(&"a".repeat(1024), CellValue::from_str(&"v".repeat(2048)))
            .is_ok()
    );
    assert_eq!(RecordOp::create(KEY).cells().len(), 0);
    assert_eq!(RecordOp::patch(KEY).changes().len(), 0);
}

#[test]
fn projections_preserve_reserved_binary_names_and_distinguish_empty_from_all() {
    assert_eq!(RecordOp::get(KEY).names(), None);
    let empty = RecordOp::get(KEY).only(std::iter::empty::<&str>());
    assert_eq!(empty.names(), Some([].as_slice()));
    let raw = [0, 255, 0, 1];
    let projection =
        RecordOp::get(KEY).only([b"#key".as_slice(), b"price", &raw, b"price", b"@version"]);
    let names = projection.names().unwrap();
    assert_eq!(names.len(), 4);
    assert_eq!(names[0].as_bytes(), &raw);
    assert_eq!(names[1].as_bytes(), b"#key");
    assert_eq!(names[2].as_bytes(), b"@version");
    assert_eq!(names[3].as_bytes(), b"price");
}

#[test]
fn builder_errors_name_the_step_and_cell_without_losing_typed_causes() {
    use golemdb_cells::{CellNameError, CellValueParseError};
    use std::error::Error;
    for (step, error) in [
        (
            "insert",
            RecordOp::create(KEY)
                .insert("", CellValue::from_bool(true))
                .unwrap_err(),
        ),
        (
            "attribute",
            RecordOp::create(KEY)
                .attribute("", CellValue::from_bool(true))
                .unwrap_err(),
        ),
        (
            "field",
            RecordOp::create(KEY)
                .field("", CellValue::from_bool(true))
                .unwrap_err(),
        ),
        (
            "set",
            RecordOp::patch(KEY)
                .set("", CellValue::from_bool(true))
                .unwrap_err(),
        ),
        (
            "attribute",
            RecordOp::patch(KEY)
                .attribute("", CellValue::from_bool(true))
                .unwrap_err(),
        ),
        (
            "field",
            RecordOp::patch(KEY)
                .field("", CellValue::from_bool(true))
                .unwrap_err(),
        ),
        ("remove", RecordOp::patch(KEY).remove("").unwrap_err()),
    ] {
        assert_eq!(
            error.to_string(),
            format!("invalid argument: {step}(\"\"): invalid cell name")
        );
        assert_eq!(
            error.source().unwrap().downcast_ref::<CellNameError>(),
            Some(&CellNameError::Empty)
        );
    }
    for error in [
        RecordOp::create(KEY)
            .attribute("payload", CellValue::from_bytes(b"x"))
            .unwrap_err(),
        RecordOp::patch(KEY)
            .attribute("payload", CellValue::from_bytes(b"x"))
            .unwrap_err(),
    ] {
        assert_eq!(
            error.to_string(),
            "invalid argument: attribute(\"payload\"): invalid cell value"
        );
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<CellValueParseError>(),
            Some(&CellValueParseError::NotIndexable)
        );
    }
    for (step, error) in [
        (
            "field",
            RecordOp::create(KEY)
                .insert("price", CellValue::from_i32(1))
                .unwrap()
                .field("price", CellValue::from_i32(2))
                .unwrap_err(),
        ),
        (
            "attribute",
            RecordOp::patch(KEY)
                .remove("price")
                .unwrap()
                .attribute("price", CellValue::from_i32(2))
                .unwrap_err(),
        ),
        (
            "remove",
            RecordOp::patch(KEY)
                .set("price", CellValue::from_i32(1))
                .unwrap()
                .remove("price")
                .unwrap_err(),
        ),
    ] {
        assert_eq!(
            error.to_string(),
            format!("invalid argument: {step}(\"price\"): duplicate cell name")
        );
        assert!(error.source().is_none());
    }
}

#[test]
fn key_and_projection_are_owned_by_the_operation() {
    let name = String::from("price");
    let read = RecordOp::get(KEY).only([name.as_str()]);
    drop(name);
    assert_eq!(read.record_key(), KEY);
    assert_eq!(read.clone(), read);
    assert_eq!(read.names().unwrap()[0].as_bytes(), b"price");
    let replacement = read.only(["#key"]);
    assert_eq!(replacement.names().unwrap().len(), 1);
    assert_eq!(replacement.names().unwrap()[0].as_bytes(), b"#key");
    let delete = RecordOp::delete(KEY);
    assert_eq!(delete.record_key(), KEY);
    assert_eq!(delete.clone(), delete);
}
