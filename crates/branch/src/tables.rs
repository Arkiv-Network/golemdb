//! Physical tables owned by the engine, updated in its transaction.
use golemdb_storage::Table;

/// Uncommitted engine rows: format identifiers and the head.
pub const SUPERBLOCK: Table = Table("Superblock");
/// Reference counts of physical trie and bitmap nodes. Derived from the
/// retained roots, so it can be rebuilt without touching committed state.
pub const NODE_REFS: Table = Table("NodeRefs");
/// Every table a database's engine state occupies; genesis requires them empty.
pub const ENGINE: [Table; 8] = [
    SUPERBLOCK,
    NODE_REFS,
    golemdb_cells::tables::CELL,
    golemdb_cells::tables::CELL_TRIE,
    golemdb_index::tables::INDEX,
    golemdb_index::tables::INDEX_TRIE,
    golemdb_index::tables::BITMAP_TRIE,
    golemdb_index::tables::BITMAP_CONTAINER,
];
