//! Data model: records, record keys, cell names, cell types and kinds, Rust
//! values, reserved records and `#meta`, observed through the record operations.
//!
//! Where the spec and the code disagree, or the spec is silent, so nothing here
//! tests it:
//! - Rust values: the spec's builders take any `impl IntoCellValue` (`bool`, `&str`,
//!   `[u8; 32]`, …, with NaN failing as a builder error); the code's builders take
//!   `CellValue` only. `CellValue::from_f32` / `from_f64` reject NaN, as tested.
//! - IDs "never reused": a rollback restores `#nextRecordID`, so a create, rollback
//!   and create again gets the same ID. The spec does not say whether the phrase
//!   covers only committed state.
//! - `#params`: the spec lists `#keyMode` and `#keySeed`; the code stores
//!   `#maxRecordCells` and `#maxRecordIndexedCells` instead, and, with immutable
//!   data configured, `#shardSpan` and `#immutableDataSegments`. A full read of
//!   `#params` is not tested.
//! - "The ID is not exposed by the API": a full read of `#recordKeys` or `#alloc`
//!   returns IDs, and these tests rely on it.
//! - `@meteringModel` and `@modelWeight`: the spec says "identity only"; the code
//!   stores metering model versions and weights.
//! - How a message shows a non-printable name: the spec is silent; the code escapes
//!   its bytes, as in `field("\xc3\xa9")` and `field("a\x00")`.
//! - `-0.0`: the spec is silent; `from_f32` / `from_f64` turn it into `+0.0`, and
//!   `CellValue::parse` rejects its encoding.
//!
//! Tested elsewhere:
//! - Record keys: `create` tests the caller-assigned mode, `AlreadyExists` and
//!   `Reserved`, and lists the stale parts (generated keys, `KeyModeMismatch`).
//!
//! Not reachable through the public API:
//! - a user record without `#meta`, or a count falling below zero (`Internal`): both
//!   need corrupted state;
//! - "`create` writes n + 3 cells and `delete` removes n + 3": no call counts cell
//!   writes. A branch's `version` counts undo entries, not cells; `patch` relies on
//!   it with a stated assumption, not repeated here. The create and delete tests
//!   observe the cells through reads.

use golemdb_conformance::*;

/// One value of each type, with its type ID and its payload width if fixed, at a
/// boundary where it has one.
fn every_type() -> Vec<(&'static str, u8, Option<usize>, CellValue)> {
    let mut i256_min = [0; 32];
    i256_min[0] = 0x80;
    vec![
        ("bool", 1, Some(1), CellValue::from_bool(true)),
        ("str", 2, None, CellValue::from_str("héllo")),
        ("bytes", 3, None, CellValue::from_bytes(&[0, 1, 255])),
        ("bytes20", 4, Some(20), CellValue::from_bytes20([0xab; 20])),
        ("bytes4", 8, Some(4), CellValue::from_bytes4([1, 2, 3, 4])),
        ("bytes8", 9, Some(8), CellValue::from_bytes8([0xff; 8])),
        ("bytes16", 10, Some(16), CellValue::from_bytes16([0; 16])),
        ("bytes32", 11, Some(32), CellValue::from_bytes32([0x5a; 32])),
        ("u32", 12, Some(4), CellValue::from_u32(u32::MAX)),
        ("u64", 13, Some(8), CellValue::from_u64(u64::MAX)),
        ("u128", 14, Some(16), CellValue::from_u128(u128::MAX)),
        ("u256", 15, Some(32), CellValue::from_u256_be([0xff; 32])),
        ("i32", 16, Some(4), CellValue::from_i32(i32::MIN)),
        ("i64", 17, Some(8), CellValue::from_i64(-1)),
        ("i128", 18, Some(16), CellValue::from_i128(i128::MIN)),
        ("i256", 19, Some(32), CellValue::from_i256_be(i256_min)),
        (
            "dec32",
            20,
            Some(4),
            CellValue::from_dec32_unscaled(-12_345),
        ),
        (
            "dec64",
            21,
            Some(8),
            CellValue::from_dec64_unscaled(i64::MAX),
        ),
        ("dec128", 22, Some(16), CellValue::from_dec128_unscaled(1)),
        (
            "dec256",
            23,
            Some(32),
            CellValue::from_dec256_unscaled_be([0xff; 32]),
        ),
        (
            "f32",
            24,
            Some(4),
            CellValue::from_f32(f32::MIN_POSITIVE).unwrap(),
        ),
        (
            "f64",
            25,
            Some(8),
            CellValue::from_f64(f64::NEG_INFINITY).unwrap(),
        ),
        ("date32", 28, Some(4), CellValue::from_date32(-719_162)),
        (
            "timestamp64",
            29,
            Some(8),
            CellValue::from_timestamp64(i64::MAX),
        ),
    ]
}

/// "**Empty records are legal:** a create may name no cells"
#[test]
fn an_empty_record_has_only_key_and_meta() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    db.create(branch, RecordOp::create(key(1), B))
        .result
        .unwrap();
    let record = db.read(ReadTarget::Branch(branch), key(1));
    let names = cell_names(&record);
    assert_eq!(names, [b"#key".as_slice(), b"#meta"]);
    // Every count is zero.
    assert_eq!(record.meta(), Some(RecordMeta::default()));
}

/// "Records are addressed internally by a dense `u64` ID, allocated from
/// `#nextRecordID`"
#[test]
fn ids_are_dense_across_commits() {
    let db = TestDb::new();
    // Two creates per branch, over two commits.
    for batch in [[(1, 64), (2, 65)], [(3, 66), (4, 67)]] {
        let branch = db.begin().unwrap();
        for (n, id) in batch {
            db.create(branch, RecordOp::create(key(n), B))
                .result
                .unwrap();
            assert_eq!(db.record_id(branch, key(n)), Some(id));
        }
        db.commit(branch).unwrap();
    }
}

/// "Records are addressed internally by a dense `u64` ID, allocated from
/// `#nextRecordID` and never reused; a re-created key gets a new ID."
#[test]
fn a_recreated_key_gets_a_new_id() {
    // Commits after the create or the delete, or neither, or both.
    for (commit_create, commit_delete) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let db = TestDb::new();
        let mut branch = db.begin().unwrap();
        let commit = |branch: &mut BranchId| {
            db.commit(*branch).unwrap();
            *branch = db.begin().unwrap();
        };
        db.create(
            branch,
            RecordOp::create(key(1), B).attribute("a", CellValue::from_u32(1)),
        )
        .result
        .unwrap();
        assert_eq!(db.record_id(branch, key(1)), Some(64));
        if commit_create {
            commit(&mut branch);
        }
        db.delete(branch, RecordOp::delete(key(1), B))
            .result
            .unwrap();
        if commit_delete {
            commit(&mut branch);
        }
        // The delete does not rewind the allocator; the re-create takes the next ID.
        assert_eq!(db.next_record_id(branch), Some(65));
        db.create(branch, RecordOp::create(key(1), B))
            .result
            .unwrap();
        assert_eq!(db.record_id(branch, key(1)), Some(65));
        assert_eq!(db.next_record_id(branch), Some(66));
        // Nothing of the old record comes back.
        let record = db.read(ReadTarget::Branch(branch), key(1));
        let names = cell_names(&record);
        assert_eq!(names, [b"#key".as_slice(), b"#meta"]);
        assert_eq!(record.meta(), Some(RecordMeta::default()));
    }
}

/// "pub struct RecordKey(pub [u8; 32]);          // From<[u8; 32]>"
#[test]
fn keys_differing_in_one_byte_are_different_records() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    // Two differ from the first in their first or their last byte only.
    let mut first = [1; 32];
    first[0] = 2;
    let mut last = [1; 32];
    last[31] = 2;
    let keys = [RecordKey::from([1; 32]), RecordKey(first), RecordKey(last)];
    for (n, k) in (0..).zip(keys) {
        db.create(
            branch,
            RecordOp::create(k, B).field("n", CellValue::from_u32(n)),
        )
        .result
        .unwrap();
    }
    for (n, k) in (0..).zip(keys) {
        assert_eq!(db.record_id(branch, k), Some(64 + u64::from(n)));
        assert_eq!(
            db.read(ReadTarget::Branch(branch), k).cells["n".as_bytes()],
            CellValue::from_u32(n)
        );
    }
}

/// "User names follow this ASCII grammar, case-sensitive:"
/// "name  = ["$"] first *rest        ; 1 .. #maxCellNameLen bytes, "$" included"
/// "first = ALPHA"
/// "rest  = ALPHA / DIGIT / "_" / "-" / "." / ":""
/// "`#` and `@` start reserved names; a user name can start with neither. `$` is
/// ordinary user syntax. Names contain no `0x00`"
/// "`Record::cells` is ordered bytewise by name."
#[test]
fn names_follow_the_grammar() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    // Empty; `#`, `@`, a digit, a `rest` symbol or a non-ASCII letter first; `$`
    // alone, twice or before a non-letter; a character outside `rest`, `0x00` and
    // the neighbours of the allowed ranges included: rejected by `create` and
    // `patch` alike. The length limit is tested with the other limits.
    db.create(branch, RecordOp::create(key(1), B))
        .result
        .unwrap();
    for name in [
        "", "#x", "@x", "1a", "_a", "-a", ".a", ":a", "é", "aé", "$", "$1", "a b", "a/b", "a$",
        "$$a", "$_a", "$#a", "$@a", "a\0", "a,b", "a;b", "a@b", "a[b", "a^b", "a`b", "a{b",
        "a\x7f",
    ] {
        // How a non-printable name is shown is unspecified: only the problem is
        // checked.
        let expected = |m: &str| {
            if name
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
            {
                m == format!("field({name:?}): invalid cell name")
            } else {
                m.ends_with("): invalid cell name")
            }
        };
        let value = || CellValue::from_u32(1);
        assert_err!(
            db.create(branch, RecordOp::create(key(2), B).field(name, value()))
                .result,
            ApiError::InvalidArgument { message, .. } if expected(&message)
        );
        assert_err!(
            db.patch(branch, RecordOp::patch(key(1), B).field(name, value()))
                .result,
            ApiError::InvalidArgument { message, .. } if expected(&message)
        );
    }
    // One letter, every `rest` character, `$` first, and case kept apart: read back
    // bytewise, `$…` before `A` before `Z` before `a`.
    let names = ["a", "A", "Z", "a0_-.:Z", "$a", "$A", "$b:c"];
    let mut create = RecordOp::create(key(2), B);
    let mut patch = RecordOp::patch(key(1), B);
    for name in names {
        create = create.field(name, CellValue::from_u32(1));
        patch = patch.field(name, CellValue::from_u32(1));
    }
    db.create(branch, create).result.unwrap();
    db.patch(branch, patch).result.unwrap();
    let mut sorted = names.map(str::as_bytes);
    sorted.sort();
    for k in [key(1), key(2)] {
        let record = db.read(ReadTarget::Branch(branch), k);
        assert_eq!(
            user_cells(&record)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            sorted
        );
    }
}

/// "A cell's value is a **type tag** byte followed by the payload. The tag's low 7
/// bits are the type ID; bit `0x80` is set for an attribute (indexed) and clear for
/// a field."
#[test]
fn every_type_reads_back_with_its_tag() {
    let types = every_type();
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    // `key(1)` holds them all as fields, `key(2)` all but `bytes` as attributes.
    let mut fields = RecordOp::create(key(1), B);
    let mut attributes = RecordOp::create(key(2), B);
    for (name, _, _, value) in types.clone() {
        if name != "bytes" {
            attributes = attributes.attribute(name, value.clone());
        }
        fields = fields.field(name, value);
    }
    db.create(branch, fields).result.unwrap();
    db.create(branch, attributes).result.unwrap();
    db.commit(branch).unwrap();
    for (k, kind_bit) in [(key(1), 0), (key(2), 0x80)] {
        let record = db.read(ReadTarget::Head, k);
        for (name, id, _, value) in types.clone() {
            let Some(stored) = record.cells.get(name.as_bytes()) else {
                assert_eq!((name, kind_bit), ("bytes", 0x80));
                continue;
            };
            assert_eq!(stored.metadata(), id | kind_bit, "{name}");
            assert_eq!(stored.value(), value.value(), "{name}");
        }
    }
}

/// "A cell's value is a **type tag** byte followed by the payload."
/// "| 1 | `bool` | 1 byte, `00` / `01` |"
/// "| 4 | `bytes20` | 20 bytes |"
/// "| 8–11 | `bytes4`, `bytes8`, `bytes16`, `bytes32` | 4 · 2^w bytes |"
#[test]
fn every_type_parses_back_at_its_width() {
    for (name, _, width, value) in every_type() {
        let mut kinds = vec![value.clone()];
        if name != "bytes" {
            kinds.push(attribute(value.clone()));
        }
        for value in kinds {
            let encoded = value.encoded_bytes().to_vec();
            assert_eq!(CellValue::parse(encoded.clone()).unwrap(), value, "{name}");
            // A fixed width parses at that width only.
            let Some(width) = width else { continue };
            assert_eq!(value.value().len(), width, "{name}");
            let short = encoded[..encoded.len() - 1].to_vec();
            let long = [encoded, vec![0]].concat();
            assert!(CellValue::parse(short).is_err(), "{name}");
            assert!(CellValue::parse(long).is_err(), "{name}");
        }
    }
}

/// "| 1 | `bool` | 1 byte, `00` / `01` |"
/// "| 2 | `str` | UTF-8, at most `#maxStrLen` bytes |"
/// "| 3 | `bytes` | as-is, at most `#maxBytesLen` bytes; **field only** |"
/// "| 4 | `bytes20` | 20 bytes |"
/// "| 8–11 | `bytes4`, `bytes8`, `bytes16`, `bytes32` | 4 · 2^w bytes |"
/// "| 12–15 | `u32` … `u256` | big-endian |"
/// "| 16–19 | `i32` … `i256` | big-endian, sign bit flipped |"
/// "| 20–23 | `dec32` … `dec256` | as `i`, at scale 4, 6, 18, 18 |"
/// "| 24, 25 | `f32`, `f64` | IEEE total order |"
/// "| 28 | `date32` | days since the Unix epoch, sign bit flipped |"
/// "| 29 | `timestamp64` | microseconds since the Unix epoch, sign bit flipped |"
#[test]
fn payloads_are_encoded_as_the_table_says() {
    // A leading byte, then `fill` up to `len` bytes.
    let flipped = |first: u8, fill: u8, len: usize| [vec![first], vec![fill; len - 1]].concat();
    let ascending: [u8; 32] = std::array::from_fn(|i| i as u8);
    let mut i256_min = [0; 32];
    i256_min[0] = 0x80;
    for (value, payload) in [
        (CellValue::from_bool(false), vec![0x00]),
        (CellValue::from_bool(true), vec![0x01]),
        (CellValue::from_str("héllo"), "héllo".as_bytes().to_vec()),
        (CellValue::from_bytes(&[0, 1, 255]), vec![0, 1, 255]),
        (CellValue::from_bytes4([1, 2, 3, 4]), vec![1, 2, 3, 4]),
        (CellValue::from_bytes8([0xab; 8]), vec![0xab; 8]),
        (
            CellValue::from_bytes16(ascending[..16].try_into().unwrap()),
            ascending[..16].to_vec(),
        ),
        (
            CellValue::from_bytes20(ascending[..20].try_into().unwrap()),
            ascending[..20].to_vec(),
        ),
        (CellValue::from_bytes32(ascending), ascending.to_vec()),
        (CellValue::from_u32(0x0102_0304), vec![1, 2, 3, 4]),
        (CellValue::from_u64(1), vec![0, 0, 0, 0, 0, 0, 0, 1]),
        (CellValue::from_i32(0), vec![0x80, 0, 0, 0]),
        (CellValue::from_i32(-1), vec![0x7f, 0xff, 0xff, 0xff]),
        (CellValue::from_i64(i64::MIN), vec![0; 8]),
        (CellValue::from_date32(0), vec![0x80, 0, 0, 0]),
        (CellValue::from_date32(-1), vec![0x7f, 0xff, 0xff, 0xff]),
        (
            CellValue::from_timestamp64(1),
            vec![0x80, 0, 0, 0, 0, 0, 0, 1],
        ),
        (CellValue::from_u128(1), [vec![0; 15], vec![1]].concat()),
        (CellValue::from_u256_be(ascending), ascending.to_vec()),
        (CellValue::from_i128(0), flipped(0x80, 0, 16)),
        (CellValue::from_i128(i128::MIN), vec![0; 16]),
        (CellValue::from_i256_be(i256_min), vec![0; 32]),
        (CellValue::from_i256_be([0xff; 32]), flipped(0x7f, 0xff, 32)),
        // `dec…` are encoded as `i…`.
        (CellValue::from_dec32_unscaled(0), vec![0x80, 0, 0, 0]),
        (CellValue::from_dec64_unscaled(-1), flipped(0x7f, 0xff, 8)),
        (CellValue::from_dec128_unscaled(i128::MIN), vec![0; 16]),
        (
            CellValue::from_dec256_unscaled_be([0xff; 32]),
            flipped(0x7f, 0xff, 32),
        ),
    ] {
        assert_eq!(value.value(), payload.as_slice(), "{value:?}");
    }
    // `dec…` carry their scale in the type.
    for (value, scale) in [
        (CellValue::from_dec32_unscaled(1), 4),
        (CellValue::from_dec64_unscaled(1), 6),
        (CellValue::from_dec128_unscaled(1), 18),
        (CellValue::from_dec256_unscaled_be([0; 32]), 18),
    ] {
        let CellType::Decimal(width) = value.cell_type() else {
            panic!("{value:?} is not a decimal");
        };
        assert_eq!(width.decimal_scale(), scale, "{value:?}");
    }
    // Floats compare by their payload bytes in numeric order, subnormals included:
    // −2 before −1 shows negative magnitudes sort in reverse.
    let f64s = [
        f64::NEG_INFINITY,
        f64::MIN,
        -2.0,
        -1.0,
        -f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE / 2.0,
        0.0,
        f64::MIN_POSITIVE / 2.0,
        f64::MIN_POSITIVE,
        1.0,
        2.0,
        f64::MAX,
        f64::INFINITY,
    ]
    .map(|f| CellValue::from_f64(f).unwrap().value().to_vec());
    assert!(f64s.is_sorted_by(|a, b| a < b));
    let f32s = [
        f32::NEG_INFINITY,
        f32::MIN,
        -2.0,
        -1.0,
        -f32::MIN_POSITIVE,
        -f32::MIN_POSITIVE / 2.0,
        0.0,
        f32::MIN_POSITIVE / 2.0,
        f32::MIN_POSITIVE,
        1.0,
        2.0,
        f32::MAX,
        f32::INFINITY,
    ]
    .map(|f| CellValue::from_f32(f).unwrap().value().to_vec());
    assert!(f32s.is_sorted_by(|a, b| a < b));
}

/// "Tag 0 is never a valid value."
/// "All other IDs up to 127 are reserved."
/// "| 1 | `bool` | 1 byte, `00` / `01` |"
/// "| 2 | `str` | UTF-8, at most `#maxStrLen` bytes |"
/// "| 3 | `bytes` | as-is, at most `#maxBytesLen` bytes; **field only** |"
#[test]
fn invalid_encodings_do_not_parse() {
    let reserved = [5, 6, 7, 26, 27].into_iter().chain(30..=127);
    // As a field or with the attribute bit.
    for tag in [0].into_iter().chain(reserved) {
        assert!(CellValue::parse(vec![tag, 0]).is_err(), "{tag}");
        assert!(CellValue::parse(vec![tag | 0x80, 0]).is_err(), "{tag}");
    }
    for bytes in [
        // No tag at all.
        vec![],
        // A `str` that is not UTF-8.
        vec![0x02, 0xff],
        // `bytes` with the attribute bit.
        vec![0x83, 0],
        // A `bool` other than `00` and `01`.
        vec![0x01, 0x02],
        // Fixed widths one byte short and one byte long: `u32`, `bytes20`.
        vec![0x0c, 0, 0, 0],
        vec![0x0c, 0, 0, 0, 0, 0],
        [vec![0x04], vec![0; 19]].concat(),
        [vec![0x04], vec![0; 21]].concat(),
    ] {
        assert!(CellValue::parse(bytes.clone()).is_err(), "{bytes:?}");
    }
}

/// "name  = ["$"] first *rest        ; 1 .. #maxCellNameLen bytes, "$" included"
/// "| 2 | `str` | UTF-8, at most `#maxStrLen` bytes |"
/// "| 3 | `bytes` | as-is, at most `#maxBytesLen` bytes; **field only** |"
/// "Fixed-width types have no configurable limit."
#[test]
fn limits_are_the_genesis_ones_in_bytes() {
    let limits = CellLimits {
        max_cell_name_len: 8,
        max_str_len: 10,
        max_bytes_len: 12,
    };
    let db = TestDb::open(limits, PricingConfig::zero());
    assert_eq!(
        cells(
            &db.get(
                ReadTarget::Head,
                RecordOp::get(reserved_key("#params"), B).only([
                    "#maxCellNameLen",
                    "#maxStrLen",
                    "#maxBytesLen"
                ]),
            )
            .result
            .unwrap()
        ),
        [
            (b"#maxBytesLen".as_slice(), &CellValue::from_u32(12)),
            (b"#maxCellNameLen", &CellValue::from_u32(8)),
            (b"#maxStrLen", &CellValue::from_u32(10)),
        ]
    );
    let branch = db.begin().unwrap();
    db.create(branch, RecordOp::create(key(1), B))
        .result
        .unwrap();
    let patch = || RecordOp::patch(key(1), B);
    let v = || CellValue::from_u32(1);
    // One byte over: a name, `$` included; a `str` of 5 two-byte characters and one
    // more byte; `bytes`.
    let str_over = || CellValue::from_str("ééééé1");
    for over in [
        patch().field("abcdefghi", v()),
        patch().field("$abcdefgh", v()),
        patch().field("s", str_over()),
        patch().attribute("s", str_over()),
        patch().field("b", CellValue::from_bytes(&[0; 13])),
    ] {
        assert_err!(
            db.patch(branch, over).result,
            ApiError::InvalidArgument { .. }
        );
    }
    assert_err!(
        db.create(branch, RecordOp::create(key(2), B).field("s", str_over()))
            .result,
        ApiError::InvalidArgument { .. }
    );
    // At the limits, an empty `str`, and fixed widths longer than `#maxBytesLen`.
    let mut i256_min = [0; 32];
    i256_min[0] = 0x80;
    db.patch(
        branch,
        patch()
            .field("abcdefgh", v())
            .field("$abcdefg", v())
            .field("s", CellValue::from_str("ééééé"))
            .attribute("t", CellValue::from_str("ééééé"))
            .field("e", CellValue::from_str(""))
            .field("b", CellValue::from_bytes(&[0; 12]))
            .attribute("h", CellValue::from_bytes32([0; 32]))
            .attribute("u", CellValue::from_u256_be([0xff; 32]))
            .attribute("i", CellValue::from_i256_be(i256_min)),
    )
    .result
    .unwrap();
}

/// "**Kind** is chosen per write, not per name: `attribute` or `field`. Two records
/// may give the same name different types and kinds."
#[test]
fn records_type_a_shared_name_independently() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key(1), B).attribute("a", CellValue::from_u32(1)),
    )
    .result
    .unwrap();
    db.create(
        branch,
        RecordOp::create(key(2), B).field("a", CellValue::from_str("x")),
    )
    .result
    .unwrap();
    db.commit(branch).unwrap();
    let a = |k| {
        db.read(ReadTarget::Head, k)
            .cells
            .get("a".as_bytes())
            .cloned()
    };
    assert_eq!(a(key(1)), Some(attribute(CellValue::from_u32(1))));
    assert_eq!(a(key(2)), Some(CellValue::from_str("x")));
}

/// "| `attribute(name, value)`, `field(name, value)` | write an indexed or a
/// stored-only cell |"
/// "| `CellValue` | its own type; the builder sets its kind |"
#[test]
fn writes_take_the_kind_the_builder_names() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    // `b` is given a value of attribute kind, but written with `field`.
    db.create(
        branch,
        RecordOp::create(key(1), B)
            .attribute("a", CellValue::from_u32(1))
            .field("b", attribute(CellValue::from_u32(1))),
    )
    .result
    .unwrap();
    // And by `patch`: `c` likewise.
    db.patch(
        branch,
        RecordOp::patch(key(1), B).field("c", attribute(CellValue::from_u32(1))),
    )
    .result
    .unwrap();
    let record = db.read(ReadTarget::Branch(branch), key(1));
    let kind = |name: &str| record.cells.get(name.as_bytes()).map(CellValue::kind);
    assert_eq!(kind("a"), Some(CellKind::Attribute));
    assert_eq!(kind("b"), Some(CellKind::Field));
    assert_eq!(kind("c"), Some(CellKind::Field));
}

/// "| `f32`, `f64` | `f32`, `f64`; NaN fails |"
#[test]
fn nan_is_not_a_value() {
    assert!(CellValue::from_f32(f32::NAN).is_err());
    assert!(CellValue::from_f64(f64::NAN).is_err());
}

/// "Records 0–63 are reserved; user records start at 64. Each reserved record's key
/// is its name, ASCII, zero-padded to 32 bytes."
/// "| 3 | `#recordKeys` | bindings: name = raw record key, value = record ID (`u64`) |"
#[test]
fn reserved_records_are_bound_at_their_ids() {
    let db = TestDb::new();
    // Each holds its zero-padded name as `#key`, a `bytes32` field.
    for name in RESERVED_NAMES {
        let k = reserved_key(name);
        assert_eq!(
            db.read(ReadTarget::Head, k).cells["#key".as_bytes()],
            CellValue::from_bytes32(k.0),
            "{name}"
        );
    }
    // At genesis, `#recordKeys` binds them, and nothing else, to their IDs.
    let record_keys = reserved_key("#recordKeys");
    let mut expected: Vec<(Vec<u8>, CellValue)> = RESERVED_NAMES
        .into_iter()
        .zip([0u64, 1, 2, 3, 4, 32, 33])
        .map(|(name, id)| (reserved_key(name).0.to_vec(), CellValue::from_u64(id)))
        .collect();
    expected.push((b"#key".to_vec(), CellValue::from_bytes32(record_keys.0)));
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    let actual: Vec<(Vec<u8>, CellValue)> = cells(&db.read(ReadTarget::Head, record_keys))
        .into_iter()
        .map(|(name, value)| (name.to_vec(), value.clone()))
        .collect();
    assert_eq!(actual, expected);
}

/// "| 2 | `#roots` | one cell per earlier commit: name = `commitNr` (`u64` BE),
/// value = `StateRoot ‖ IndexRoot` (64-byte `bytes` field), written lag-one by each
/// commit |"
#[test]
fn roots_holds_one_cell_per_earlier_commit() {
    let db = TestDb::new();
    let roots = reserved_key("#roots");
    let names = |record: &Record| -> Vec<Vec<u8>> {
        cells(record)
            .into_iter()
            .map(|(name, _)| name.to_vec())
            .collect()
    };
    // At genesis there is no earlier commit.
    assert_eq!(names(&db.read(ReadTarget::Head, roots)), [b"#key".to_vec()]);

    // Commits 1 and 2; each writes the cell of the commit before it.
    let mut sealed = Vec::new();
    for _ in 0..2 {
        let branch = db.begin().unwrap();
        sealed.push(db.seal(branch).unwrap());
        db.commit(branch).unwrap();
    }
    let record = db.read(ReadTarget::Head, roots);
    let commit_nr = |n: u64| n.to_be_bytes().to_vec();
    assert_eq!(
        names(&record),
        [commit_nr(0), commit_nr(1), b"#key".to_vec()]
    );

    // Each is a field holding that commit's state root then its index root:
    // genesis's from opening, commit 1's from sealing.
    let genesis = db.info();
    assert_eq!(genesis.commit_id, CommitId::GENESIS);
    assert_eq!(sealed[0].commit_id, CommitId::new(1));
    for (n, state_root, index_root) in [
        (0u64, genesis.state_root, genesis.index_root),
        (1, sealed[0].state_root, sealed[0].index_root),
    ] {
        let value = &record.cells[n.to_be_bytes().as_slice()];
        assert_eq!(value.kind(), CellKind::Field);
        assert_eq!(
            value.as_bytes(),
            Some([state_root, index_root].concat().as_slice())
        );
    }

    // The raw name projects the cell alone.
    let projected = db
        .get(
            ReadTarget::Head,
            RecordOp::get(roots, B).only([0u64.to_be_bytes().as_slice()]),
        )
        .result
        .unwrap();
    assert_eq!(names(&projected), [commit_nr(0)]);
}

/// "| 4 | `#rootIndex` | identity only |"
#[test]
fn root_index_holds_only_its_identity() {
    let db = TestDb::new();
    let root_index = reserved_key("#rootIndex");
    assert_eq!(
        cells(&db.read(ReadTarget::Head, root_index)),
        [(b"#key".as_slice(), &CellValue::from_bytes32(root_index.0))]
    );
}

/// "`get` works on reserved records like on any other; a full read returns all
/// their cells."
#[test]
fn reserved_records_read_like_any_other() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    let alloc = reserved_key("#alloc");
    let params = reserved_key("#params");
    for target in [
        ReadTarget::Head,
        ReadTarget::Branch(branch),
        ReadTarget::Commit(CommitId::GENESIS),
    ] {
        assert_eq!(
            cells(&db.read(target, alloc)),
            [
                (b"#key".as_slice(), &CellValue::from_bytes32(alloc.0)),
                (b"#nextRecordID", &CellValue::from_u64(64)),
            ]
        );
        // A projection works on them too.
        let projected = db
            .get(
                target,
                RecordOp::get(params, B).only(["#maxCellNameLen", "#maxStrLen", "#maxBytesLen"]),
            )
            .result
            .unwrap();
        assert_eq!(
            cells(&projected),
            [
                (b"#maxBytesLen".as_slice(), &CellValue::from_u32(128)),
                (b"#maxCellNameLen", &CellValue::from_u32(32)),
                (b"#maxStrLen", &CellValue::from_u32(64)),
            ]
        );
        // So does an empty one.
        assert!(
            db.get(
                target,
                RecordOp::get(params, B).only(std::iter::empty::<&str>())
            )
            .result
            .unwrap()
            .cells
            .is_empty()
        );
    }
}

/// "| `cells` | user cells |"
/// "| `cell_bytes` | Σ `(8 + |name|) + (1 + |value|)`: cell key plus encoded value |"
/// "| `indexed_cells` | user cells of kind attribute |"
/// "| `index_bytes` | Σ over attributes of `|name| + 2 + |value|`: the index term key |"
/// "**Encoding:** the four fields in this order, each `u64` BE, 32 bytes, stored as a
/// `bytes32` field."
#[test]
fn meta_counts_the_user_cells() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key(1), B)
            .attribute("aa", CellValue::from_u32(1))
            .field("bbb", CellValue::from_str("xy"))
            .attribute("h", CellValue::from_bytes32([0; 32]))
            .field("dddd", CellValue::from_bytes(&[0; 5]))
            .attribute("eeeee", CellValue::from_str("aé"))
            .field("zz", CellValue::from_bytes(&[])),
    )
    .result
    .unwrap();
    // Attributes: `aa` is (8 + 2) + (1 + 4), indexed as 2 + 2 + 4; `h` is
    // (8 + 1) + (1 + 32), indexed as 1 + 2 + 32; `eeeee` is (8 + 5) + (1 + 3),
    // indexed as 5 + 2 + 3: "aé" is 3 bytes, though 2 characters.
    // Fields: `bbb` is (8 + 3) + (1 + 2); `dddd` is (8 + 4) + (1 + 5); the empty `zz`
    // is (8 + 2) + (1 + 0).
    let (cells, cell_bytes, indexed_cells, index_bytes) =
        (6u64, 15 + 42 + 17 + 14 + 18 + 11, 3, 8 + 35 + 10);
    let record = db.read(ReadTarget::Branch(branch), key(1));
    assert_eq!(
        record.meta(),
        Some(RecordMeta {
            cells,
            cell_bytes,
            indexed_cells,
            index_bytes
        })
    );
    // Stored as the four counts, big-endian, in that order.
    let encoded: Vec<u8> = [cells, cell_bytes, indexed_cells, index_bytes]
        .iter()
        .flat_map(|count| count.to_be_bytes())
        .collect();
    assert_eq!(
        record.cells["#meta".as_bytes()],
        CellValue::from_bytes32(encoded.try_into().unwrap())
    );
}

/// "`Record::meta()` decodes it, returning `None` if the read did not include it."
#[test]
fn meta_decodes_only_when_read() {
    let db = TestDb::new();
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key(1), B).attribute("a", CellValue::from_u32(1)),
    )
    .result
    .unwrap();
    db.commit(branch).unwrap();
    let meta = |names: &[&str]| {
        db.get(
            ReadTarget::Head,
            RecordOp::get(key(1), B).only(names.iter().copied()),
        )
        .result
        .unwrap()
        .meta()
    };
    let full = db.read(ReadTarget::Head, key(1)).meta();
    assert!(full.is_some());
    assert_eq!(meta(&["#meta"]), full);
    assert_eq!(meta(&["a"]), None);
}
