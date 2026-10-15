//! Immutable segment addressing. Storage is not implemented yet: every operation
//! on Database returns ApiError::NotImplemented without inspecting or changing state.

/// Dense, segment-local append position. An append's result is provisional until
/// its sealed branch commits; losing or discarded branches publish no position.
pub type ImmutableDataOrdinal = u64;

/// One opaque byte array per column. Column arity belongs to the future segment
/// declaration; rows have no cell types, attribute flags, or embedded key column.
pub type ImmutableDataRow = Vec<Vec<u8>>;

/// Optional caller-assigned identity, unique within a segment. Separate from
/// record keys; the same bytes can identify rows in different segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImmutableDataKey(pub [u8; 32]);

/// Both addresses identify the same committed row. A key is additional addressing
/// metadata; every row still has an ordinal, including rows appended without keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImmutableDataAddress {
    Ordinal(ImmutableDataOrdinal),
    Key(ImmutableDataKey),
}
