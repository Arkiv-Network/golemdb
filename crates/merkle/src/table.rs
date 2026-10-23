use golemdb_storage::Table;

/// A table of trie rows, declared once with the width `NS` of the namespace
/// that follows the commit in each row key: 0 for a table of one trie, or the
/// width shared by the namespaces of the tries in it. Keys of namespaces of
/// different widths can coincide, as a shorter namespace and a route can spell
/// a longer namespace, so every trie in the table takes its width from here.
///
/// ```compile_fail,E0308
/// use golemdb_merkle::{Keccak256Hasher, Trie, TrieTable};
/// use golemdb_storage::Table;
/// const EXAMPLE_BRANCH_DOMAIN: u8 = 0x03;
/// const TERMS: TrieTable<1> = TrieTable::new(Table("Terms"));
/// let _ = Trie::<_, 6, 2>::namespaced(TERMS, [1, 2], EXAMPLE_BRANCH_DOMAIN, &Keccak256Hasher);
/// ```
///
/// ```compile_fail,E0308
/// use golemdb_merkle::{Keccak256Hasher, Trie, TrieTable};
/// use golemdb_storage::Table;
/// const EXAMPLE_BRANCH_DOMAIN: u8 = 0x03;
/// const TERMS: TrieTable<1> = TrieTable::new(Table("Terms"));
/// let _ = Trie::<_, 6>::new(TERMS, EXAMPLE_BRANCH_DOMAIN, &Keccak256Hasher);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrieTable<const NS: usize = 0>(Table);

impl<const NS: usize> TrieTable<NS> {
    pub const fn new(table: Table) -> Self {
        Self(table)
    }

    /// The physical table, for reading or writing its rows directly.
    pub const fn table(self) -> Table {
        self.0
    }
}
