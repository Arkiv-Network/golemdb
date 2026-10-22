use std::{borrow::Cow, fmt};

/// A name in a pricing schedule or receipt ledger, ordered by its exact text.
///
/// Names are open-ended so future models and host weights do not change the
/// receipt type. The constants are the weights this write model actually uses.
/// Declaration/form checks at genesis and activation belong to their loader;
/// constructing a name does not declare or price it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WeightName(Cow<'static, str>);

impl WeightName {
    pub const WRITE_ADMISSION_BASE: Self = Self(Cow::Borrowed("write.admission.base"));
    pub const WRITE_ADMISSION_ENTRY: Self = Self(Cow::Borrowed("write.admission.entry"));
    pub const WRITE_ADMISSION_BYTE: Self = Self(Cow::Borrowed("write.admission.byte"));
    pub const WRITE_RECORD_CREATE: Self = Self(Cow::Borrowed("write.record.create"));
    pub const WRITE_RECORD_PATCH: Self = Self(Cow::Borrowed("write.record.patch"));
    pub const WRITE_RECORD_DELETE: Self = Self(Cow::Borrowed("write.record.delete"));
    pub const WRITE_CELL_CREATE: Self = Self(Cow::Borrowed("write.cell.create"));
    pub const WRITE_CELL_UPDATE: Self = Self(Cow::Borrowed("write.cell.update"));
    pub const WRITE_CELL_DELETE: Self = Self(Cow::Borrowed("write.cell.delete"));
    pub const WRITE_CELL_TRIE_UPDATE: Self = Self(Cow::Borrowed("write.cell_trie_update"));
    pub const WRITE_CELL_BYTE: Self = Self(Cow::Borrowed("write.cell_byte"));
    pub const WRITE_INDEX_JOIN: Self = Self(Cow::Borrowed("write.index.join"));
    pub const WRITE_INDEX_LEAVE: Self = Self(Cow::Borrowed("write.index.leave"));
    pub const WRITE_INDEX_TERM_CREATE: Self = Self(Cow::Borrowed("write.index_term_create"));
    pub const WRITE_INDEX_TRIE_UPDATE: Self = Self(Cow::Borrowed("write.index_trie_update"));
    pub const WRITE_INDEX_BYTE: Self = Self(Cow::Borrowed("write.index_byte"));
    pub const READ_KEY: Self = Self(Cow::Borrowed("read.key"));
    pub const READ_CELL: Self = Self(Cow::Borrowed("read.cell"));
    pub const READ_TERM: Self = Self(Cow::Borrowed("read.term"));

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for WeightName {
    fn from(name: &str) -> Self {
        Self(Cow::Owned(name.to_owned()))
    }
}

impl From<String> for WeightName {
    fn from(name: String) -> Self {
        Self(Cow::Owned(name))
    }
}

impl std::borrow::Borrow<str> for WeightName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for WeightName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
