use golemdb_cells::CellType;

use crate::{HI48_BYTES, IndexError, IndexTerm, PATH_BYTE_OFFSET, TermError, path};

/// A container's `Index` key, decoded: its term and its `container_id`, the
/// `hi48` its record IDs share. Encoded, it is the term's key (its bytes, a
/// string value ended by `0x00`), then `hi48`. Keys are in term order, so a
/// term's containers are contiguous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexKey {
    pub term: IndexTerm,
    pub container_id: u64,
}

impl IndexKey {
    /// `termKey ‖ hi48`.
    pub fn encode(&self) -> crate::Result<Vec<u8>> {
        let mut key = term_key(self.term.as_bytes());
        key.extend_from_slice(&path::path_bytes(self.container_id)?);
        Ok(key)
    }

    /// Decode a stored key, rejecting a malformed or noncanonical term or a
    /// suffix that is not a `hi48`. The term ends after its fixed-width value,
    /// or after a string's `0x00`.
    pub fn decode(key: &[u8]) -> crate::Result<Self> {
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
                let end = start + end.iter().position(|&b| b == 0).ok_or_else(bad)?;
                (&key[..end], &key[end + 1..])
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
        name_len + 2 + value + HI48_BYTES as u64
    }
}

/// The term key every `Index` key of a term starts with, from the term's
/// bytes ([`IndexTerm::as_bytes`]): a variable-width value is followed by
/// `0x00`, which no such value holds. No other term's key starts with it, and
/// term key order is term order.
pub(crate) fn term_key(term_bytes: &[u8]) -> Vec<u8> {
    let tag = term_bytes
        .iter()
        .position(|&b| b == 0)
        .and_then(|split| term_bytes.get(split + 1));
    if tag.and_then(|&tag| value_width(tag)).is_some() {
        return term_bytes.to_vec();
    }
    [term_bytes, &[0]].concat()
}

/// The `container_id` stored as `hi48`, the bytes after a term key; any other
/// length is a corrupt key.
pub(crate) fn container_id(hi48: &[u8]) -> crate::Result<u64> {
    let hi48: [u8; HI48_BYTES] =
        (hi48.try_into()).map_err(|_| IndexError::Corruption("malformed container key"))?;
    let mut be = [0; 8];
    be[PATH_BYTE_OFFSET..].copy_from_slice(&hi48);
    Ok(u64::from_be_bytes(be))
}

/// The width of the value after a type tag, or `None` for a variable width
/// or an unknown tag.
fn value_width(tag: u8) -> Option<usize> {
    let ty = CellType::from_id(tag & 0x7f)
        .ok()
        .filter(|_| tag & 0x80 != 0)?;
    ty.width()
}
