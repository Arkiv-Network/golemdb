//! Physical table names shared with the engine. Writes belong to one transaction.
use golemdb_storage::Table;

pub const INDEX: Table = Table("Index");
pub const BITMAP_CONTAINER: Table = Table("BitmapContainer");
pub const BITMAP_TRIE: Table = Table("BitmapTrie");
pub const INDEX_TRIE: Table = Table("IndexTrie");

/// Tables keyed by writing commit first; each commit's rows are appended.
pub const VERSIONED: [Table; 3] = [INDEX_TRIE, BITMAP_TRIE, BITMAP_CONTAINER];
