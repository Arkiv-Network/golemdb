//! Physical tables shared with the engine, updated in its transaction.

use golemdb_history::HistoryTables;
use golemdb_merkle::TrieTable;
use golemdb_storage::Table;

pub const CELL: Table = Table("Cell");
/// Modifying commits, keyed by the complete encoded cell key.
pub const CELL_HISTORY: Table = Table("CellHistory");
/// Tagged pre-images (empty bytes mean absence), keyed by commit then cell key.
pub const CELL_CHANGE_SET: Table = Table("CellChangeSet");
pub const HISTORY: HistoryTables = HistoryTables::new(CELL_HISTORY, CELL_CHANGE_SET);
/// The cell trie's branches.
pub const CELL_TRIE: TrieTable = TrieTable::new(Table("CellTrie"));

/// Tables keyed by writing commit first; each commit's rows are appended.
pub const VERSIONED: [Table; 1] = [CELL_TRIE.table()];
