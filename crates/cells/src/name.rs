//! [`CellName`] and [`CellNameRef`]: cell names and the §3 grammar.
//!
//! ```text
//! name  = ["$"] first *rest              ; 1 .. #maxCellNameLen bytes
//! first = ALPHA
//! rest  = ALPHA / DIGIT / "_" / "-" / "." / ":"
//! ```
//!
//! ASCII only and case-sensitive (`Price` ≠ `price`). No `0x00`, so the index
//! key's `0x00` separator is unambiguous. A leading `$` is ordinary user
//! syntax; `#` and `@` are reserved for engine names.

use core::borrow::Borrow;
use core::fmt;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellNameError {
    Empty,
    /// Longer than the caller's `#maxCellNameLen`.
    TooLong {
        max: usize,
        actual: usize,
    },
    /// The first byte is not a letter: a digit, a separator, `#` or `@`.
    NotAlphaFirst(u8),
    /// A byte outside the grammar, at offset `at` in the whole key.
    InvalidByte {
        at: usize,
        byte: u8,
    },
}

impl fmt::Display for CellNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "cell name is empty"),
            Self::TooLong { max, actual } => {
                write!(f, "cell name is {actual} bytes, the maximum is {max}")
            }
            Self::NotAlphaFirst(b) => write!(f, "cell name must begin with a letter, got {b:#04x}"),
            Self::InvalidByte { at, byte } => {
                write!(f, "byte {byte:#04x} at offset {at} is not a name character")
            }
        }
    }
}

impl core::error::Error for CellNameError {}

/// `first *rest`. `base` shifts error offsets, so a name behind a sigil
/// reports positions in the whole key: `#max Len` fails at offset 4.
fn check(name: &[u8], base: usize) -> Result<(), CellNameError> {
    let [first, rest @ ..] = name else {
        return Err(CellNameError::Empty);
    };
    if !first.is_ascii_alphabetic() {
        return Err(CellNameError::NotAlphaFirst(*first));
    }
    let is_rest = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':');
    match rest.iter().position(|b| !is_rest(b)) {
        Some(i) => Err(CellNameError::InvalidByte {
            at: base + 1 + i,
            byte: rest[i],
        }),
        None => Ok(()),
    }
}

/// A borrowed within-record name. User and named engine constructors validate
/// their grammar; `raw` preserves arbitrary reserved-record keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellNameRef<'a>(&'a [u8]);

impl<'a> CellNameRef<'a> {
    /// A user name: the grammar, capped at `max_len` (`#maxCellNameLen`).
    pub fn parse_user(name: &'a [u8], max_len: usize) -> Result<Self, CellNameError> {
        if name.len() > max_len {
            return Err(CellNameError::TooLong {
                max: max_len,
                actual: name.len(),
            });
        }
        if let Some(rest) = name.strip_prefix(b"$") {
            check(rest, 1)?;
        } else {
            check(name, 0)?;
        }
        Ok(Self(name))
    }

    /// An engine name: `#` or `@`, then the grammar. No cap; engine names are
    /// constants.
    pub fn parse_engine(name: &'a [u8]) -> Result<Self, CellNameError> {
        match name {
            [] => Err(CellNameError::Empty),
            [b'#' | b'@', rest @ ..] => check(rest, 1).map(|()| Self(name)),
            [byte, ..] => Err(CellNameError::InvalidByte { at: 0, byte: *byte }),
        }
    }

    /// A reserved record's key (§4), which is itself a value such as a
    /// `commitNr`, so no grammar applies. Engine-internal.
    pub const fn raw(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> &'a [u8] {
        self.0
    }
}

impl fmt::Display for CellNameRef<'_> {
    /// Escaped, because `raw` keys need not be ASCII.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.escape_ascii().fmt(f)
    }
}

/// An owned cell name. `Ord` is bytewise, matching MDBX, so a
/// `BTreeMap<CellName, _>` iterates in table order; `Borrow<[u8]>` lets it be
/// probed with a plain slice.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellName(Box<[u8]>);

impl CellName {
    /// Borrow the name bytes without allocating or copying.
    pub fn as_view(&self) -> CellNameRef<'_> {
        CellNameRef(&self.0)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// A projection's names: taken raw, deduplicated, in byte order. Raw so
    /// that reserved and binary names stay selectable, as reads allow.
    pub fn projection<I, N>(names: I) -> Vec<Self>
    where
        I: IntoIterator<Item = N>,
        N: AsRef<[u8]>,
    {
        names
            .into_iter()
            .map(|name| Self::from(CellNameRef::raw(name.as_ref())))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

impl Borrow<[u8]> for CellName {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for CellName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        CellNameRef(&self.0).fmt(f)
    }
}

impl From<CellNameRef<'_>> for CellName {
    fn from(key: CellNameRef<'_>) -> Self {
        Self(key.0.into())
    }
}

/// The engine's reserved cell names (§3, §4).
pub mod reserved {
    use super::CellNameRef;

    /// A record's logical key.
    pub const KEY: CellNameRef<'static> = CellNameRef(b"#key");
    /// The next record ID to hand out.
    pub const NEXT_RECORD_ID: CellNameRef<'static> = CellNameRef(b"#nextRecordID");
    /// Chain parameter: the longest `str` value.
    pub const MAX_STR_LEN: CellNameRef<'static> = CellNameRef(b"#maxStrLen");
    /// Chain parameter: the longest `bytes` value.
    pub const MAX_BYTES_LEN: CellNameRef<'static> = CellNameRef(b"#maxBytesLen");
    /// Chain parameter: the cap [`CellNameRef::parse_user`] is given.
    pub const MAX_CELL_NAME_LEN: CellNameRef<'static> = CellNameRef(b"#maxCellNameLen");

    pub const ALL: &[CellNameRef<'static>] = &[
        KEY,
        NEXT_RECORD_ID,
        MAX_STR_LEN,
        MAX_BYTES_LEN,
        MAX_CELL_NAME_LEN,
    ];
}
