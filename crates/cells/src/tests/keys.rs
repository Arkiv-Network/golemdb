//! The cell-name tables and the assertions over them: the §3 grammar, the
//! engine `#`/`@` names, and the unvalidated `raw` keys.

use std::collections::BTreeMap;

use proptest::prelude::*;

use crate::*;

/// Explicit fixture cap, not a default deployment limit. Long enough that
/// only the `max_len` rows below are near it.
const TEST_MAX_CELL_NAME_LEN: usize = 64;

/// Names `parse_user` must accept, at `TEST_MAX_CELL_NAME_LEN`.
#[rustfmt::skip]
const ACCEPT: &[&str] = &[
    "$owner",
    "$a.b:c-d_e",
    "Price",            // case is significant …
    "price",            // … so this is a different cell
    "snake_case",
    "camelCase",
    "price.usd",
    "erc20:balance",
    "created-at",
    "a",                // one byte is a whole name
    "a.b:c-d_e",        // every separator at once
    "x9",               // a digit is fine anywhere but the front
    // §3 puts no structural rules on separators — "versatility over
    // tidiness". These two are deliberate, not oversights: a trailing
    // separator and a doubled one are both valid names.
    "trailing.",
    "a..b",
];

/// Names `parse_user` must reject, each with the error it must give.
#[rustfmt::skip]
const REJECT: &[(&str, &[u8], CellNameError)] = &[
    ("empty",             b"",            CellNameError::Empty),
    ("leading digit",     b"9lives",      CellNameError::NotAlphaFirst(b'9')),
    ("leading separator", b"_private",    CellNameError::NotAlphaFirst(b'_')),
    // Engine prefixes cannot be user names.
    ("engine sigil #",    b"#key",        CellNameError::NotAlphaFirst(b'#')),
    ("admin sigil @",     b"@admin",      CellNameError::NotAlphaFirst(b'@')),
    ("dollar alone",      b"$",           CellNameError::Empty),
    ("double dollar",     b"$$x",         CellNameError::NotAlphaFirst(b'$')),
    ("dollar digit",      b"$1",          CellNameError::NotAlphaFirst(b'1')),
    ("interior dollar",   b"a$b",         CellNameError::InvalidByte { at: 1, byte: b'$' }),
    ("prefixed space",    b"$a b",        CellNameError::InvalidByte { at: 2, byte: b' ' }),
    ("space",             b"pri ce",      CellNameError::InvalidByte { at: 3, byte: b' ' }),
    ("NUL",               b"pri\x00ce",   CellNameError::InvalidByte { at: 3, byte: 0x00 }),
    // "café" — the é is two bytes, and neither is ASCII.
    ("non-ASCII",         b"caf\xc3\xa9", CellNameError::InvalidByte { at: 3, byte: 0xC3 }),
    ("punctuation",       b"price!",      CellNameError::InvalidByte { at: 5, byte: b'!' }),
];

#[test]
fn user_names_accept_the_grammar() {
    for name in ACCEPT {
        let key = CellNameRef::parse_user(name.as_bytes(), TEST_MAX_CELL_NAME_LEN)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(key.as_bytes(), name.as_bytes(), "{name}");
    }
}

#[test]
fn user_names_reject_for_the_stated_reason() {
    for (label, name, expected) in REJECT {
        let got = CellNameRef::parse_user(name, TEST_MAX_CELL_NAME_LEN)
            .map(|k| k.as_bytes())
            .expect_err(&format!("{label}: parsed, should have failed"));
        assert_eq!(got, *expected, "{label}");
    }
}

/// The cap is the caller's, not a constant baked into the validator: the same
/// name passes under one cap and fails under a smaller one.
#[test]
fn length_cap_comes_from_the_caller() {
    let name = vec![b'a'; TEST_MAX_CELL_NAME_LEN];
    assert!(
        CellNameRef::parse_user(&name, TEST_MAX_CELL_NAME_LEN).is_ok(),
        "exactly max_len"
    );

    let over = vec![b'a'; TEST_MAX_CELL_NAME_LEN + 1];
    assert_eq!(
        CellNameRef::parse_user(&over, TEST_MAX_CELL_NAME_LEN),
        Err(CellNameError::TooLong {
            max: TEST_MAX_CELL_NAME_LEN,
            actual: TEST_MAX_CELL_NAME_LEN + 1
        })
    );

    // A name accepted at 64 is rejected at 4, with no change to the name.
    assert!(CellNameRef::parse_user(b"erc20:balance", 64).is_ok());
    assert_eq!(
        CellNameRef::parse_user(b"erc20:balance", 4),
        Err(CellNameError::TooLong { max: 4, actual: 13 })
    );
}

#[test]
fn engine_names_take_a_sigil_then_the_grammar() {
    for key in reserved::ALL {
        assert_eq!(
            CellNameRef::parse_engine(key.as_bytes()).map(|k| k.as_bytes()),
            Ok(key.as_bytes()),
            "{key}"
        );
        // …and the data plane cannot reach any of them.
        assert!(
            CellNameRef::parse_user(key.as_bytes(), TEST_MAX_CELL_NAME_LEN).is_err(),
            "{key}"
        );
    }

    assert!(CellNameRef::parse_engine(b"@admin").is_ok());
    assert!(CellNameRef::parse_engine(b"$owner").is_err());
    // The name behind the sigil obeys the same rules, offsets included.
    assert_eq!(CellNameRef::parse_engine(b"#"), Err(CellNameError::Empty));
    assert_eq!(
        CellNameRef::parse_engine(b"#9lives"),
        Err(CellNameError::NotAlphaFirst(b'9'))
    );
    assert_eq!(
        CellNameRef::parse_engine(b"#max Len"),
        Err(CellNameError::InvalidByte { at: 4, byte: b' ' })
    );
    // A sigil is required; a plain user name is not an engine name.
    assert_eq!(
        CellNameRef::parse_engine(b"price"),
        Err(CellNameError::InvalidByte { at: 0, byte: b'p' })
    );
    assert_eq!(CellNameRef::parse_engine(b""), Err(CellNameError::Empty));
}

/// The reserved constants are spelled as §3/§4 spell them.
#[test]
fn reserved_names_are_the_spec_s() {
    assert_eq!(reserved::KEY.as_bytes(), b"#key");
    assert_eq!(reserved::NEXT_RECORD_ID.as_bytes(), b"#nextRecordID");
    assert_eq!(reserved::MAX_STR_LEN.as_bytes(), b"#maxStrLen");
    assert_eq!(reserved::MAX_BYTES_LEN.as_bytes(), b"#maxBytesLen");
    assert_eq!(reserved::MAX_CELL_NAME_LEN.as_bytes(), b"#maxCellNameLen");
}

/// `raw` validates nothing: reserved records (§4) use the cell key as a value
/// — a commitNr, a recordKey — and no naming rule applies inside them.
#[test]
fn raw_keys_bypass_the_grammar() {
    for bytes in [
        &[][..],
        &7u64.to_be_bytes()[..],
        &[0xFF; 32][..],
        b"9 !\x00",
    ] {
        assert_eq!(CellNameRef::raw(bytes).as_bytes(), bytes);
    }
}

/// Bytewise order, matching MDBX, so a `BTreeMap` keyed by `CellName` walks in
/// table order. `Borrow<[u8]>` lets that map be probed without allocating.
#[test]
fn owned_keys_order_bytewise_and_borrow_as_bytes() {
    let mut map = BTreeMap::new();
    for name in ["price", "Price", "a", "price.usd", "priceX"] {
        let key = CellName::from(
            CellNameRef::parse_user(name.as_bytes(), TEST_MAX_CELL_NAME_LEN).unwrap(),
        );
        map.insert(key, name);
    }

    let order: Vec<&[u8]> = map.keys().map(|k| k.as_bytes()).collect();
    let mut expected = order.clone();
    expected.sort_unstable();
    assert_eq!(order, expected, "iteration is not bytewise");
    // 'P' < 'a', and '.' (0x2E) < 'X' (0x58) — plain byte comparison, no
    // case folding and no separator special-casing.
    assert_eq!(order[0], b"Price");
    assert_eq!(order.last().unwrap(), b"priceX");

    // Probed with a bare slice: no CellName allocated for the lookup.
    assert_eq!(map.get(b"price.usd".as_slice()), Some(&"price.usd"));
    assert_eq!(map.get(b"missing".as_slice()), None);
}

#[test]
fn owned_and_borrowed_round_trip() {
    let key = CellNameRef::parse_user(b"erc20:balance", TEST_MAX_CELL_NAME_LEN).unwrap();
    let owned: CellName = key.into();
    assert_eq!(owned.as_bytes(), key.as_bytes());
    assert_eq!(owned.to_string(), "erc20:balance");
    // The escape is what makes a `raw` key printable at all.
    assert_eq!(CellNameRef::raw(&[0x00, 0xFF]).to_string(), r"\x00\xff");
}

proptest! {
    /// Whatever the validator accepts is ASCII, starts with an optional `$` followed by a letter, and
    /// carries no `0x00` — the last is what keeps the `Index` key's `0x00`
    /// separator unambiguous, and it is asserted here rather than inferred
    /// from the grammar.
    #[test]
    fn accepted_names_are_ascii_letter_led_and_nul_free(
        name in prop::collection::vec(any::<u8>(), 0..80),
        max_len in 0usize..80,
    ) {
        if let Ok(key) = CellNameRef::parse_user(&name, max_len) {
            let bytes = key.as_bytes();
            prop_assert_eq!(bytes, &name[..]);
            prop_assert!(!bytes.is_empty());
            prop_assert!(bytes.len() <= max_len);
            prop_assert!(bytes.is_ascii());
            prop_assert!(bytes.strip_prefix(b"$").unwrap_or(bytes)[0].is_ascii_alphabetic());
            prop_assert!(!bytes.contains(&0x00));
        }
    }

    /// An engine name is a sigil plus a name that would be valid on its own,
    /// and never collides with the user name space.
    #[test]
    fn engine_names_are_a_sigil_plus_a_user_name(name in prop::collection::vec(any::<u8>(), 0..80)) {
        if let Ok(key) = CellNameRef::parse_engine(&name) {
            let bytes = key.as_bytes();
            prop_assert!(matches!(bytes[0], b'#' | b'@'));
            prop_assert!(CellNameRef::parse_user(&bytes[1..], bytes.len()).is_ok());
            prop_assert!(CellNameRef::parse_user(bytes, bytes.len()).is_err());
        }
    }
}

#[test]
fn projections_are_raw_deduplicated_and_bytewise() {
    let names = CellName::projection(["status", "#key", "Owner", "status", "\u{ff}"]);
    let bytes: Vec<&[u8]> = names.iter().map(CellName::as_bytes).collect();
    assert_eq!(
        bytes,
        [&b"#key"[..], b"Owner", b"status", "\u{ff}".as_bytes()]
    );
    assert!(CellName::projection(Vec::<&str>::new()).is_empty());
}
