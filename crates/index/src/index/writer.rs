use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{Table, WriteTransaction};

use super::Index;
use crate::{
    BitmapContainer, INDEX_TRIE_PATH_BYTES, IndexKey, IndexTerm, Result,
    index_trie::{ContainerEdit, IndexTriePath},
    path, tables,
};

/// Adds a record ID to a term's bitmap or removes it from that bitmap.
/// If a batch changes the same term and record ID more than once, the last change wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostingChange {
    Add { term: IndexTerm, record_id: u64 },
    Remove { term: IndexTerm, record_id: u64 },
}

/// A [`PostingChange`] with its record ID split into container and offset.
struct Posting {
    term: IndexTerm,
    container: u64,
    offset: u16,
    present: bool,
}

impl Posting {
    /// What identifies a posting, without whether it is present.
    fn key(&self) -> (&IndexTerm, u64, u16) {
        (&self.term, self.container, self.offset)
    }
}

impl From<PostingChange> for Posting {
    fn from(change: PostingChange) -> Self {
        let (term, record_id, present) = match change {
            PostingChange::Add { term, record_id } => (term, record_id, true),
            PostingChange::Remove { term, record_id } => (term, record_id, false),
        };
        let (container, offset) = path::split(record_id);
        Self {
            term,
            container,
            offset,
            present,
        }
    }
}

/// The result of applying a batch: the new index root and the terms whose postings changed.
/// These changes become permanent only when the caller commits the transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexUpdate {
    pub root: RootRef<INDEX_TRIE_PATH_BYTES>,
    /// The terms whose postings changed, in term order.
    pub changed_terms: Vec<IndexTerm>,
    /// Stored trie rows this batch replaced, which only older roots reach.
    pub superseded: Vec<(Table, Vec<u8>)>,
}

impl<H: HashProvider> Index<'_, H> {
    /// Apply postings with last-operation-wins semantics for each (term, ID).
    /// Coalesces containers, rewrites each changed container's row in place,
    /// and updates the trie once. The caller must supply the root matching this
    /// transaction's tables; each touched container is checked against it in
    /// that same trie update, but this is not a whole-index audit. New trie
    /// rows are stored under `commit`, the commit this batch publishes; a
    /// second batch at the same commit replaces rows, so only its root stays
    /// valid.
    pub fn apply(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<INDEX_TRIE_PATH_BYTES>,
        changes: impl IntoIterator<Item = PostingChange>,
    ) -> Result<IndexUpdate> {
        let mut batch: Vec<_> = changes.into_iter().map(Posting::from).collect();
        // Stable sorting preserves input order for each posting. Do not include
        // `present` in the key: the final input operation determines membership.
        batch.sort_by(|a, b| a.key().cmp(&b.key()));
        batch.dedup_by(|later, earlier| {
            if later.key() == earlier.key() {
                // dedup_by keeps the earlier entry; copy the last operation into it.
                earlier.present = later.present;
                true
            } else {
                false
            }
        });
        let mut edits = Vec::new();
        let mut changed_terms = Vec::new();
        for group in batch.chunk_by(|a, b| a.term == b.term) {
            let term = &group[0].term;
            let term_path = term.routing_path(self.hasher);
            let mut changed = false;
            for postings in group.chunk_by(|a, b| a.container == b.container) {
                let container = postings[0].container;
                let key = IndexKey {
                    term: term.clone(),
                    container_id: container,
                }
                .encode()?;
                let path = IndexTriePath::new(&term_path, container)?;
                let stored = tx.get(tables::INDEX, &key)?;
                let before = stored
                    .as_ref()
                    .map(|bytes| self.trie.leaf_hash(&path, bytes));
                let mut bitmap = match &stored {
                    Some(bytes) => BitmapContainer::decode(container, bytes)?,
                    None => BitmapContainer::empty(container)?,
                };
                let mut dirty = false;
                for posting in postings {
                    dirty |= if posting.present {
                        bitmap.insert(posting.offset)
                    } else {
                        bitmap.remove(posting.offset)
                    };
                }
                // Unchanged containers are still checked against the root.
                let mut after = before;
                if dirty {
                    changed = true;
                    let bytes = (!bitmap.is_empty()).then(|| bitmap.encode()).transpose()?;
                    after = bytes
                        .as_ref()
                        .map(|bytes| self.trie.leaf_hash(&path, bytes));
                    match bytes {
                        Some(bytes) => tx.put(tables::INDEX, &key, &bytes)?,
                        None => {
                            tx.delete(tables::INDEX, &key)?;
                        }
                    }
                }
                edits.push(ContainerEdit {
                    path,
                    before,
                    after,
                });
            }
            if changed {
                changed_terms.push(term.clone());
            }
        }
        let update = self.trie.set(tx, commit, root, edits)?;
        Ok(IndexUpdate {
            root: update.root,
            changed_terms,
            superseded: update.superseded,
        })
    }
}
