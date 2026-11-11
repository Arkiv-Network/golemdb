use std::ops::Bound;

use golemdb_cells::CellType;

use crate::{ContainerId, IndexError, IndexTerm, TermError, term_range::past};

/// A container's `Index` key, decoded: its term and its `container_id`, the
/// `hi48` its record IDs share. Encoded, it is the term's bytes, then `hi48`.
/// No term is a prefix of another, so keys are in term order and a term's
/// containers are contiguous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexKey {
    pub term: IndexTerm,
    pub container_id: ContainerId,
}

impl IndexKey {
    /// `term ‖ hi48`.
    pub fn encode(&self) -> Vec<u8> {
        encode(&self.term, self.container_id)
    }

    /// Decode a stored key, rejecting a malformed or noncanonical term or a
    /// suffix that is not a `hi48`. The term ends after its fixed-width value,
    /// or after a string's `0x00`.
    pub fn decode(key: &[u8]) -> crate::Result<Self> {
        // Finds where the term ends by the rule `IndexTerm::decode` checks;
        // that decode is the source of truth and validates the bytes found.
        let bad = || TermError::InvalidEncoding;
        let split = key.iter().position(|&b| b == 0).ok_or_else(bad)?;
        let tag = *key.get(split + 1).ok_or_else(bad)?;
        let start = split + 2;
        let (term, hi48) = match value_width(tag) {
            Some(width) => {
                let end = start.checked_add(width).filter(|&end| end <= key.len());
                let end = end.ok_or_else(bad)?;
                (&key[..end], &key[end..])
            }
            None => {
                let end = key.get(start..).ok_or_else(bad)?;
                let end = start + end.iter().position(|&b| b == 0).ok_or_else(bad)? + 1;
                (&key[..end], &key[end..])
            }
        };
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
        name_len + 2 + value + ContainerId::BYTES as u64
    }
}

/// The `Index` key of `term`'s container `container_id`: `term ‖ hi48`.
pub(crate) fn encode(term: &IndexTerm, container_id: ContainerId) -> Vec<u8> {
    [term.as_bytes(), &container_id.to_bytes()].concat()
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
pub(crate) fn container_id(hi48: &[u8]) -> crate::Result<ContainerId> {
    ContainerId::from_slice(hi48).ok_or(IndexError::Corruption("malformed Index key"))
}

/// The width of the value after a type tag, or `None` for a variable width
/// or an unknown tag.
fn value_width(tag: u8) -> Option<usize> {
    let ty = CellType::from_id(tag & 0x7f)
        .ok()
        .filter(|_| tag & 0x80 != 0)?;
    ty.width()
}
