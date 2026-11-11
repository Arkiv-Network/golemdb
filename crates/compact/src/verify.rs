use std::ops::Bound;

use golemdb_branch::{head_roots, read_head_state};
use golemdb_cells::{CELL_BRANCH_DOMAIN, tables::CELL_TRIE};
use golemdb_index::{
    INDEX_BRANCH_DOMAIN,
    tables::{INDEX, INDEX_TRIE},
};
use golemdb_merkle::{HashProvider, RootRef, Trie};
use golemdb_storage::{Store, scan};

use crate::{PruneError, Result, mark::bitmap_trie};

/// Read the head in full with the engine's readers. Runs after the deletions,
/// so it reports a wrongly deleted row rather than preventing it.
pub(crate) fn verify_head<H: HashProvider>(store: &impl Store, hasher: &H) -> Result<()> {
    let read = store.begin_read()?;
    let roots = head_roots(&read, &read_head_state(&read)?, hasher)?;
    for leaf in Trie::new(CELL_TRIE, CELL_BRANCH_DOMAIN, hasher).walk(&read, roots.state) {
        leaf?;
    }
    for leaf in Trie::new(INDEX_TRIE, INDEX_BRANCH_DOMAIN, hasher).walk(&read, roots.index) {
        leaf?;
    }
    for row in scan(&read, INDEX, Bound::Unbounded, Bound::Unbounded)? {
        let (term, root) = row?;
        let root = RootRef::decode(&root).map_err(|_| PruneError::Corrupt("Index row"))?;
        let trie = bitmap_trie(hasher, &term);
        for leaf in trie.walk(&read, root) {
            trie.payload(&read, leaf?)?
                .ok_or(PruneError::Corrupt("missing container"))?;
        }
    }
    Ok(())
}
