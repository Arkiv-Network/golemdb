//! Physical table names shared with the engine. Writes belong to one transaction.
use golemdb_history::HistoryTables;
use golemdb_merkle::TrieTable;
use golemdb_storage::Table;

/// Live containers keyed by [`crate::IndexKey`], `term ‖ hi48`,
/// each holding its canonical Roaring bytes: the payloads of the index trie's
/// leaves. Keys are in term order, so a term's containers are contiguous and
/// a term exists exactly while it has a container.
pub const INDEX: Table = Table("Index");
/// Modifying commits, keyed by the complete term and container ID.
pub const INDEX_HISTORY: Table = Table("IndexHistory");
/// Container pre-images (empty bytes mean absence), keyed by commit then IndexKey.
pub const INDEX_CHANGE_SET: Table = Table("IndexChangeSet");
pub const HISTORY: HistoryTables = HistoryTables::new(INDEX_HISTORY, INDEX_CHANGE_SET);
/// The index trie's branches.
pub const INDEX_TRIE: TrieTable = TrieTable::new(Table("IndexTrie"));

/// Tables keyed by writing commit first; each commit's rows are appended.
pub const VERSIONED: [Table; 1] = [INDEX_TRIE.table()];
