//! Physical table names shared with the engine. Writes belong to one transaction.
use golemdb_merkle::{HASH_BYTES, TrieTable};
use golemdb_storage::Table;

pub const INDEX: Table = Table("Index");
/// Every term's bitmap containers, namespaced by the term's routing path.
pub const BITMAP_CONTAINER: TrieTable<HASH_BYTES> = TrieTable::new(Table("BitmapContainer"));
/// Every term's bitmap trie branches, namespaced by the term's routing path.
pub const BITMAP_TRIE: TrieTable<HASH_BYTES> = TrieTable::new(Table("BitmapTrie"));
/// The one index trie's branches, unnamespaced.
pub const INDEX_TRIE: TrieTable = TrieTable::new(Table("IndexTrie"));

/// Tables keyed by writing commit first; each commit's rows are appended.
pub const VERSIONED: [Table; 3] = [
    INDEX_TRIE.table(),
    BITMAP_TRIE.table(),
    BITMAP_CONTAINER.table(),
];
