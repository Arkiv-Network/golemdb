/// Segment-global append position. A staged position is provisional until
/// commit; truncating or discarding its batch allows that position to be reused.
pub type ImmutableDataOrdinal = u64;

/// Optional caller-assigned identity, scoped to one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImmutableDataKey(pub [u8; 32]);

/// Two ways to address the same committed row. Every row has an ordinal,
/// including rows appended without a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImmutableDataAddress {
    Ordinal(ImmutableDataOrdinal),
    Key(ImmutableDataKey),
}

/// Column selection in caller order. `None` selects the complete schema;
/// an explicit empty list is invalid. A schema validates names and duplicates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImmutableDataReadOptions {
    pub columns: Option<Vec<String>>,
}

impl ImmutableDataReadOptions {
    pub const fn all() -> Self {
        Self { columns: None }
    }

    pub fn columns(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            columns: Some(names.into_iter().map(Into::into).collect()),
        }
    }
}
