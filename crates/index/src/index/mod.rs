use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::ReadTransaction;

use crate::{
    BITMAP_TRIE_PATH_BYTES, IndexTerm, Result,
    bitmap_trie::BitmapTrie,
    index_trie::{IndexTrie, decode_bitmap_root},
    tables,
};

mod reader;
mod writer;

pub use reader::TermScan;
pub use writer::{IndexUpdate, PostingChange, TermRootChange};

/// Index operations over caller-owned transactions. Use the same hash provider
/// and snapshot for the flat table, both tries, and the supplied root. Errors
/// during `apply` require aborting the transaction. Publish roots only on commit.
pub struct Index<'h, H: HashProvider> {
    hasher: &'h H,
    bitmaps: BitmapTrie<'h, H>,
    trie: IndexTrie<'h, H>,
}

impl<'h, H: HashProvider> Index<'h, H> {
    pub fn new(hasher: &'h H) -> Self {
        Self {
            hasher,
            bitmaps: BitmapTrie::new(hasher),
            trie: IndexTrie::new(hasher),
        }
    }

    /// Store new trie rows and containers at `commit`, the commit this batch
    /// publishes.
    pub fn at(self, commit: u64) -> Self {
        Self {
            bitmaps: self.bitmaps.at(commit),
            trie: self.trie.at(commit),
            ..self
        }
    }

    fn bitmap_root(
        &self,
        tx: &impl ReadTransaction,
        term: &IndexTerm,
    ) -> Result<Option<RootRef<BITMAP_TRIE_PATH_BYTES>>> {
        tx.get(tables::INDEX, term.as_bytes())?
            .map(|bytes| decode_bitmap_root(&bytes, self.hasher))
            .transpose()
    }
}
