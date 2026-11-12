use std::ops::Bound;

use crate::{
    HI48_BYTES, IndexError, IndexTerm, PATH_BYTE_OFFSET, TermError, path, term_range::past,
};

/// A container's `Index` key, decoded: its term and its `container_id`, the
/// `hi48` its record IDs share. Encoded, it is the term's bytes, then `hi48`.
/// No term is a prefix of another, so keys are in term order and a term's
/// containers are contiguous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexKey {
    pub term: IndexTerm,
    pub container_id: u64,
}

impl IndexKey {
    /// `term ‖ hi48`.
    pub fn encode(&self) -> crate::Result<Vec<u8>> {
        encode(&self.term, self.container_id)
    }

    /// Decode a stored key: a canonical term followed by its container's
    /// `hi48`.
    pub fn decode(key: &[u8]) -> crate::Result<Self> {
        let (term, hi48) = key
            .split_last_chunk::<HI48_BYTES>()
            .ok_or(TermError::InvalidEncoding)?;
        Ok(Self {
            term: IndexTerm::decode(term)?,
            container_id: container_id(hi48)?,
        })
    }

    /// The longest key of a term whose name has `name_len` bytes and whose
    /// value has at most `value_len` bytes, or 32 if fixed-width: a string's
    /// terminator, then `hi48`, follow the value.
    pub const fn max_len(name_len: u64, value_len: u64) -> u64 {
        let string = value_len + 1;
        let value = if string > 32 { string } else { 32 };
        name_len + 2 + value + HI48_BYTES as u64
    }
}

/// The `Index` key of `term`'s container `container_id`: `term ‖ hi48`.
pub(crate) fn encode(term: &IndexTerm, container_id: u64) -> crate::Result<Vec<u8>> {
    Ok([term.as_bytes(), &path::path_bytes(container_id)?].concat())
}

/// Bounds on `Index` keys holding exactly the containers of the terms within
/// `terms` ([`crate::TermRange::bounds`]). Every key of a term starts with
/// it, so an included lower or excluded upper term bounds the keys as is; an
/// excluded lower term starts past all its keys, and an included upper one
/// ends past them.
pub(crate) fn bounds(
    (lower, upper): (Bound<&[u8]>, Bound<&[u8]>),
) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let lower = match lower {
        Bound::Included(term) => Bound::Included(term.to_vec()),
        Bound::Excluded(term) => after(term),
        Bound::Unbounded => Bound::Unbounded,
    };
    let upper = match upper {
        Bound::Included(term) => past(term.to_vec()),
        Bound::Excluded(term) => Bound::Excluded(term.to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    };
    (lower, upper)
}

/// The lower bound just past every key that starts with `term`.
fn after(term: &[u8]) -> Bound<Vec<u8>> {
    match past(term.to_vec()) {
        Bound::Excluded(next) => Bound::Included(next),
        // Only an all-0xff prefix has no key after it, and every term
        // holds the 0x00 after its name.
        _ => unreachable!("a term holds a 0x00 byte"),
    }
}

/// The `container_id` stored as `hi48`, the bytes after a term; any other
/// length is a corrupt key.
pub(crate) fn container_id(hi48: &[u8]) -> crate::Result<u64> {
    let hi48: [u8; HI48_BYTES] =
        (hi48.try_into()).map_err(|_| IndexError::Corruption("malformed container key"))?;
    let mut be = [0; 8];
    be[PATH_BYTE_OFFSET..].copy_from_slice(&hi48);
    Ok(u64::from_be_bytes(be))
}
