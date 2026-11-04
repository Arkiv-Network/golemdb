//! Physical tables shared with the engine, updated in its transaction.

use golemdb_merkle::TrieTable;
use golemdb_storage::Table;

pub const CELL: Table = Table("Cell");
/// The cell trie's branches.
pub const CELL_TRIE: TrieTable = TrieTable::new(Table("CellTrie"));

/// Tables keyed by writing commit first; each commit's rows are appended.
pub const VERSIONED: [Table; 1] = [CELL_TRIE.table()];
