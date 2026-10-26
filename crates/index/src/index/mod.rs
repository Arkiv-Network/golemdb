use golemdb_merkle::HashProvider;

use crate::index_trie::IndexTrie;

mod reader;
mod writer;

pub use reader::TermScan;
pub use writer::{IndexUpdate, PostingChange, TermChange};

/// Index operations over caller-owned transactions. Use the same hash provider
/// and snapshot for the tables, the trie, and the supplied root. Errors during
/// `apply` require aborting the transaction. Publish roots only on commit.
pub struct Index<'h, H: HashProvider> {
    hasher: &'h H,
    trie: IndexTrie<'h, H>,
}

impl<'h, H: HashProvider> Index<'h, H> {
    pub fn new(hasher: &'h H) -> Self {
        Self {
            hasher,
            trie: IndexTrie::new(hasher),
        }
    }
}
