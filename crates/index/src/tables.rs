//! Physical table names shared with the engine. Writes belong to one transaction.
use golemdb_merkle::TrieTable;
use golemdb_storage::Table;

/// Live containers keyed by `term key ‖ path` ([`crate::IndexTerm::key`]),
/// each holding its canonical Roaring bytes: the payloads of the index trie's
/// leaves. Keys are in term order, so a term's containers are contiguous and
/// a term exists exactly while it has a container.
pub const INDEX: Table = Table("Index");
/// The index trie's branches.
pub const INDEX_TRIE: TrieTable = TrieTable::new(Table("IndexTrie"));

/// Tables keyed by writing commit first; each commit's rows are appended.
pub const VERSIONED: [Table; 1] = [INDEX_TRIE.table()];
