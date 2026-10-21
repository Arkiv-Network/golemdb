use crate::{BITMAP_BRANCH_DOMAIN, BITMAP_LEAF_DOMAIN};
use golemdb_merkle::{Hash, HashProvider, LeafPayload, LeafRef, PayloadTrie, RootRef};
use golemdb_storage::{ReadTransaction, WriteTransaction};

use crate::{
    BITMAP_TRIE_PATH_BYTES, Bitmap, BitmapContainer, BitmapError, IndexError, Result, path, tables,
};

/// One term's bitmap trie, namespaced by the term's routing path. Containers
/// are its leaves' payloads, stored under `commit ‖ term ‖ container path`.
pub(crate) struct BitmapTrie<'a, H: HashProvider> {
    trie: PayloadTrie<'a, H, BITMAP_TRIE_PATH_BYTES>,
    hasher: &'a H,
}

impl<'a, H: HashProvider> BitmapTrie<'a, H> {
    /// The bitmap trie of the term whose routing path is `term`.
    pub(crate) fn new(hasher: &'a H, term: &'a Hash) -> Self {
        Self {
            trie: PayloadTrie::new(
                tables::BITMAP_TRIE,
                tables::BITMAP_CONTAINER,
                BITMAP_BRANCH_DOMAIN,
                hasher,
            )
            .in_namespace(term),
            hasher,
        }
    }

    fn decode(&self, hash: Hash, bytes: &[u8]) -> Result<BitmapContainer> {
        if self.hasher.hash_parts(&[&[BITMAP_LEAF_DOMAIN], bytes]) != hash {
            return Err(IndexError::Corruption("container hash mismatch"));
        }
        Ok(BitmapContainer::decode(bytes)?)
    }

    fn load(
        &self,
        tx: &impl ReadTransaction,
        leaf: LeafRef<BITMAP_TRIE_PATH_BYTES>,
    ) -> Result<BitmapContainer> {
        let bytes =
            (self.trie.payload(tx, leaf)?).ok_or(IndexError::Corruption("missing container"))?;
        let container = self.decode(leaf.hash, &bytes)?;
        if path::path_bytes(container.path())? != leaf.path {
            return Err(IndexError::Corruption(
                "container path disagrees with leaf reference",
            ));
        }
        Ok(container)
    }

    pub(crate) fn bitmap(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<BITMAP_TRIE_PATH_BYTES>,
    ) -> Result<Bitmap> {
        let containers = self
            .trie
            .walk(tx, root)
            .map(|leaf| self.load(tx, leaf?))
            .collect::<Result<Vec<_>>>()?;
        Ok(Bitmap::from_containers(containers)?)
    }

    /// Changes must be sorted by (container path, offset), with one final desired
    /// membership per posting. The index writer sorts and deduplicates the batch.
    /// Consecutive changes share one container load and finalization, and the
    /// changed containers update the trie in one batch.
    pub(crate) fn apply(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        before: Option<RootRef<BITMAP_TRIE_PATH_BYTES>>,
        changes: impl IntoIterator<Item = (u64, u16, bool)>,
    ) -> Result<Option<RootRef<BITMAP_TRIE_PATH_BYTES>>> {
        let root = before.unwrap_or(RootRef::Empty);
        let mut changes = changes.into_iter().peekable();
        let mut edits = Vec::new();
        let mut previous = None;
        while let Some(&(hi, _, _)) = changes.peek() {
            // Each container is visited once, so the original root answers as
            // the evolving root would.
            if let Some(previous) = previous.filter(|&previous| previous >= hi) {
                return Err(BitmapError::UnsortedPaths { previous, path: hi }.into());
            }
            previous = Some(hi);
            let path = path::path_bytes(hi)?;
            let mut container = match self.trie.get(tx, root, &path)? {
                Some(leaf) => self.load(tx, leaf)?,
                None => BitmapContainer::empty(hi)?,
            };
            let mut changed = false;
            while let Some((_, offset, present)) = changes.next_if(|&(path, _, _)| path == hi) {
                changed |= if present {
                    container.insert(offset)
                } else {
                    container.remove(offset)
                };
            }
            if !changed {
                continue;
            }
            if container.is_empty() {
                edits.push((path, None));
            } else {
                let bytes = container.canonical_bytes()?;
                let hash = self.hasher.hash_parts(&[&[BITMAP_LEAF_DOMAIN], &bytes]);
                edits.push((path, Some(LeafPayload { hash, bytes })));
            }
        }
        let root = self.trie.apply(tx, commit, root, edits)?;
        Ok((root != RootRef::Empty).then_some(root))
    }
}
