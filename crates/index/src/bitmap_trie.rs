use crate::{BITMAP_BRANCH_DOMAIN, BITMAP_LEAF_DOMAIN};
use golemdb_merkle::{Hash, HashProvider, LeafRef, RootRef, Trie, version_key};
use golemdb_storage::{ReadTransaction, WriteTransaction};

use crate::{
    BITMAP_TRIE_PATH_BYTES, Bitmap, BitmapContainer, BitmapError, IndexError, Result, path, tables,
};

/// One trie per term, namespaced by the term's routing path. Containers are
/// its leaves, stored as rows under `commit ‖ term ‖ container path`.
pub(crate) struct BitmapTrie<'h, H: HashProvider> {
    hasher: &'h H,
}

impl<'h, H: HashProvider> BitmapTrie<'h, H> {
    pub(crate) fn new(hasher: &'h H) -> Self {
        Self { hasher }
    }

    fn trie(&self, term: &Hash) -> Trie<'h, H, BITMAP_TRIE_PATH_BYTES> {
        Trie::new(tables::BITMAP_TRIE, BITMAP_BRANCH_DOMAIN, self.hasher)
            .in_namespace(term)
            .with_leaf_rows()
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
        term: &Hash,
        leaf: LeafRef<BITMAP_TRIE_PATH_BYTES>,
    ) -> Result<BitmapContainer> {
        let bytes = tx
            .get(
                tables::BITMAP_CONTAINER,
                &version_key(leaf.commit, term, &leaf.path),
            )?
            .ok_or(IndexError::Corruption("missing container"))?;
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
        term: &Hash,
        root: RootRef<BITMAP_TRIE_PATH_BYTES>,
    ) -> Result<Bitmap> {
        let containers = self
            .trie(term)
            .walk(tx, root)
            .map(|leaf| self.load(tx, term, leaf?))
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
        term: &Hash,
        before: Option<RootRef<BITMAP_TRIE_PATH_BYTES>>,
        changes: impl IntoIterator<Item = (u64, u16, bool)>,
    ) -> Result<Option<RootRef<BITMAP_TRIE_PATH_BYTES>>> {
        let trie = self.trie(term);
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
            let mut container = match trie.get(tx, root, &path)? {
                Some(leaf) => self.load(tx, term, leaf)?,
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
                tx.put(
                    tables::BITMAP_CONTAINER,
                    &version_key(commit, term, &path),
                    &bytes,
                )?;
                edits.push((path, Some(hash)));
            }
        }
        let root = trie.apply(tx, commit, root, edits)?;
        Ok((root != RootRef::Empty).then_some(root))
    }
}
