use golemdb_storage::Table;

/// A table of one trie's branch rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrieTable(Table);

impl TrieTable {
    pub const fn new(table: Table) -> Self {
        Self(table)
    }

    /// The physical table, for reading or writing its rows directly.
    pub const fn table(self) -> Table {
        self.0
    }
}
