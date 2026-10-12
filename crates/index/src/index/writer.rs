use golemdb_merkle::{Hash, HashProvider, RootRef};
use golemdb_storage::WriteTransaction;

use super::Index;
use crate::{INDEX_TRIE_PATH_BYTES, IndexTerm, Result, path, tables};

/// Adds a record ID to a term's bitmap or removes it from that bitmap.
/// If a batch changes the same term and record ID more than once, the last change wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostingChange {
    Add { term: IndexTerm, record_id: u64 },
    Remove { term: IndexTerm, record_id: u64 },
}

/// A term's bitmap root before and after a batch of changes.
/// `None` means the term had no records at that point and was absent from the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermRootChange {
    pub term: IndexTerm,
    pub before: Option<Hash>,
    pub after: Option<Hash>,
}

/// The result of applying a batch: the new index root and the terms whose bitmaps changed.
/// These changes become permanent only when the caller commits the transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexUpdate {
    pub root: RootRef<INDEX_TRIE_PATH_BYTES>,
    /// Actual changes only, in term order. Roots span the complete input batch.
    pub changed_terms: Vec<TermRootChange>,
}

impl<H: HashProvider> Index<'_, H> {
    /// Apply postings with last-operation-wins semantics for each (term, ID).
    /// Coalesces containers and updates each affected term leaf once. The caller
    /// must supply the root matching this transaction's flat state; affected
    /// term commitments are checked, but this is not a whole-index audit.
    pub fn apply(
        &self,
        tx: &mut impl WriteTransaction,
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
        let (mut changed_terms, mut bitmaps) = (Vec::new(), Vec::new());
        for group in changes.chunk_by(|a, b| a.0 == b.0) {
            let term = &group[0].0;
            let before_root = self.bitmap_root(tx, term)?;
            let before = before_root.map(|root| root.hash(self.hasher));
            self.trie.check(tx, root, term, before)?;
            let after_root = self.bitmaps.apply(
                tx,
                &term.routing_path(self.hasher),
                before_root,
                group.iter().map(|&(_, hi, lo, present)| (hi, lo, present)),
            )?;
            let after = after_root.map(|root| root.hash(self.hasher));
            if before == after {
                continue;
            }
            changed_terms.push(TermRootChange {
                term: term.clone(),
                before,
                after,
            });
            bitmaps.push(after_root);
        }
        // Terms are unique, so each check above saw the original root; one trie
        // update for the whole batch writes each touched branch once.
        let root = self.trie.set(
            tx,
            root,
            changed_terms
                .iter()
                .map(|change| (&change.term, change.after)),
        )?;
        for (change, bitmap) in changed_terms.iter().zip(bitmaps) {
            match bitmap {
                Some(bitmap) => tx.put(tables::INDEX, change.term.as_bytes(), &bitmap.encode())?,
                None => {
                    tx.delete(tables::INDEX, change.term.as_bytes())?;
                }
            }
        }
        Ok(IndexUpdate {
            root,
            changed_terms,
        })
    }
}
