use golemdb_merkle::{CheckedEdit, HashProvider, RootRef};
use golemdb_storage::{ReadTransaction, WriteTransaction, scan_prefix};

use super::Index;
use crate::{
    BitmapContainer, INDEX_TRIE_PATH_BYTES, IndexTerm, Result,
    index_trie::{row_key, trie_path},
    path, tables,
};

/// Adds a record ID to a term's bitmap or removes it from that bitmap.
/// If a batch changes the same term and record ID more than once, the last change wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostingChange {
    Add { term: IndexTerm, record_id: u64 },
    Remove { term: IndexTerm, record_id: u64 },
}

/// A term whose postings a batch changed, and whether it had postings before
/// and after: a term without postings is absent from the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermChange {
    pub term: IndexTerm,
    pub before: bool,
    pub after: bool,
}

/// The result of applying a batch: the new index root and the terms whose postings changed.
/// These changes become permanent only when the caller commits the transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexUpdate {
    pub root: RootRef<INDEX_TRIE_PATH_BYTES>,
    /// Actual changes only, in term order.
    pub changed_terms: Vec<TermChange>,
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
        let mut changes: Vec<_> = changes
            .into_iter()
            .map(|change| {
                let (term, id, present) = match change {
                    PostingChange::Add { term, record_id } => (term, record_id, true),
                    PostingChange::Remove { term, record_id } => (term, record_id, false),
                };
                let (hi, lo) = path::split(id);
                (term, hi, lo, present)
            })
            .collect();
        // Stable sorting preserves input order for each posting. Do not include
        // `present` in the key: the final input operation determines membership.
        changes.sort_by(|a, b| (&a.0, a.1, a.2).cmp(&(&b.0, b.1, b.2)));
        changes.dedup_by(|later, earlier| {
            if (&later.0, later.1, later.2) == (&earlier.0, earlier.1, earlier.2) {
                // dedup_by keeps the earlier entry; copy the last operation into it.
                earlier.3 = later.3;
                true
            } else {
                false
            }
        });
        let mut edits = Vec::new();
        let mut changed_terms = Vec::new();
        for group in changes.chunk_by(|a, b| a.0 == b.0) {
            let term = &group[0].0;
            let term_key = term.key();
            let term_path = term.routing_path(self.hasher);
            // Whether a touched container held postings before, and after.
            let (mut touched_before, mut touched_after, mut changed) = (false, false, false);
            let (mut keys, mut writes) = (Vec::new(), Vec::new());
            for container in group.chunk_by(|a, b| a.1 == b.1) {
                let hi = container[0].1;
                let key = row_key(&term_key, hi)?;
                let path = trie_path(&term_path, hi)?;
                let stored = tx.get(tables::INDEX, &key)?;
                keys.push(key.clone());
                let before = stored
                    .as_ref()
                    .map(|bytes| self.trie.leaf_hash(&path, bytes));
                touched_before |= stored.is_some();
                let mut bitmap = match &stored {
                    Some(bytes) => BitmapContainer::decode_values(hi, bytes)?,
                    None => BitmapContainer::empty(hi)?,
                };
                let mut dirty = false;
                for &(_, _, offset, present) in container {
                    dirty |= if present {
                        bitmap.insert(offset)
                    } else {
                        bitmap.remove(offset)
                    };
                }
                touched_after |= !bitmap.is_empty();
                // Unchanged containers are still checked against the root.
                let mut after = before;
                if dirty {
                    changed = true;
                    let bytes = (!bitmap.is_empty())
                        .then(|| bitmap.value_bytes())
                        .transpose()?;
                    after = bytes
                        .as_ref()
                        .map(|bytes| self.trie.leaf_hash(&path, bytes));
                    writes.push((key, bytes));
                }
                edits.push(CheckedEdit {
                    path,
                    before,
                    after,
                });
            }
            if !changed {
                continue;
            }
            // Untouched containers are unchanged by the batch: look for one
            // only when the touched ones do not settle a side.
            let untouched = if touched_before && touched_after {
                false
            } else {
                untouched_exists(tx, &term_key, &keys)?
            };
            for (key, bytes) in writes {
                match bytes {
                    Some(bytes) => tx.put(tables::INDEX, &key, &bytes)?,
                    None => {
                        tx.delete(tables::INDEX, &key)?;
                    }
                }
            }
            let (before, after) = (touched_before || untouched, touched_after || untouched);
            changed_terms.push(TermChange {
                term: term.clone(),
                before,
                after,
            });
        }
        let root = self.trie.set(tx, commit, root, edits)?;
        Ok(IndexUpdate {
            root,
            changed_terms,
        })
    }
}

/// Whether the term with key `term_key` has a container row besides those at
/// `touched`, the sorted keys of the containers a batch touches. Reads at
/// most one row past the touched ones.
fn untouched_exists(
    tx: &impl ReadTransaction,
    term_key: &[u8],
    touched: &[Vec<u8>],
) -> Result<bool> {
    for row in scan_prefix(tx, tables::INDEX, term_key.to_vec())? {
        if touched.binary_search(&row?.0).is_err() {
            return Ok(true);
        }
    }
    Ok(false)
}
