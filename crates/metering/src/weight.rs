use std::{borrow::Cow, fmt};

/// A name in a pricing schedule or receipt ledger, ordered by its exact text.
///
/// Names are open-ended so future models and host weights do not change the
/// receipt type. The constants declare GolemDB code version 1's weight names,
/// including query/log weights whose execution metering is still deferred.
/// Declaration/form checks at genesis and activation belong to their loader;
/// constructing a name does not declare or price it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WeightName(Cow<'static, str>);

// Keep pricing constants and the configuration catalogue on one declaration.
macro_rules! define_weights {
    ($($constant:ident => $name:literal),* $(,)?) => {
        impl WeightName {
            $(pub const $constant: Self = Self(Cow::Borrowed($name));)*
        }

        /// Names declared by GolemDB code version 1 (metering D10). Declaring
        /// query/log names permits configuration before their execution metering.
        pub const GOLEMDB_WEIGHT_NAMES: &[&str] = &[$($name),*];
    };
}

define_weights! {
    WRITE_ADMISSION_BASE => "write.admission.base",
    WRITE_ADMISSION_ENTRY => "write.admission.entry",
    WRITE_ADMISSION_BYTE => "write.admission.byte",
    WRITE_RECORD_CREATE => "write.record.create",
    WRITE_RECORD_PATCH => "write.record.patch",
    WRITE_RECORD_DELETE => "write.record.delete",
    WRITE_CELL_CREATE => "write.cell.create",
    WRITE_CELL_UPDATE => "write.cell.update",
    WRITE_CELL_DELETE => "write.cell.delete",
    WRITE_CELL_TRIE_UPDATE => "write.cell_trie_update",
    WRITE_CELL_BYTE => "write.cell_byte",
    WRITE_INDEX_JOIN => "write.index.join",
    WRITE_INDEX_LEAVE => "write.index.leave",
    WRITE_INDEX_TERM_CREATE => "write.index_term_create",
    WRITE_INDEX_TRIE_UPDATE => "write.index_trie_update",
    WRITE_INDEX_BYTE => "write.index_byte",
    READ_KEY => "read.key",
    READ_CELL => "read.cell",
    READ_CELL_SCAN => "read.cell_scan",
    READ_BYTE => "read.byte",
    READ_TERM => "read.term",
    READ_BASE => "read.base",
    READ_ADMISSION_BASE => "read.admission.base",
    READ_ADMISSION_ENTRY => "read.admission.entry",
    READ_ADMISSION_BYTE => "read.admission.byte",
    READ_TERM_STEP => "read.term_step",
    READ_CONTAINER => "read.container",
    READ_SET_CONTAINER => "read.set_container",
    READ_SORT_COMPARE => "read.sort_compare",
    WRITE_LOG_APPEND_BASE => "write.log_append.base",
    WRITE_LOG_APPEND_BYTE => "write.log_append.byte",
    READ_LOG_ROW => "read.log_row",
}

impl WeightName {
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
