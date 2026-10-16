use golemdb_cells::{
    CellChange, CellKey, CellNameRef, CellType, CellValue, CellValueRef, Cells, CellsUpdate,
    system, tables,
};
use golemdb_index::{Index, IndexTerm, IndexUpdate, PostingChange};
use golemdb_merkle::{Hash, HashProvider};
use golemdb_storage::ReadTransaction;

use crate::{
    BranchError, CommitId, Result,
    buffer::{Buffered, Writes},
    head::read_head_state,
    overlay::CellOverlay,
    roots::head_roots,
};

/// Computed next state, retained in memory until commit or discard.
/// The commit number is provisional: sealing does not reserve or advance head.
/// Returned through `Arc` so observing the result does not copy the cell diff.
#[derive(Debug)]
pub struct SealedCommit {
    pub commit_id: CommitId,
    pub state_root: Hash,
    pub index_root: Hash,
    pub cells: CellsUpdate,
    pub index: IndexUpdate,
    // Replayed by commit inside the head-checked writer.
    pub(crate) writes: Writes,
}

pub(crate) fn compute(
    origin: &impl ReadTransaction,
    overlay: &CellOverlay,
    hasher: &impl HashProvider,
) -> Result<SealedCommit> {
    let head = read_head_state(origin)?;
    let commit_id = head
        .commit_id
        .get()
        .checked_add(1)
        .map(crate::CommitId::new)
        .ok_or(BranchError::CommitNumberExhausted)?;
    // New trie rows are keyed by this commit. Competing seals share its number,
    // but only the published one leaves its buffer.
    let commit = commit_id.get();
    let cells = Cells::new(hasher);
    let index = Index::new(hasher);
    // Reopen against the committed origin, before applying ANY staged cell.
    let roots = head_roots(origin, &head, hasher)?;
    let cell_root = cells.reopen(origin, roots.state)?;
    let index_root = index.reopen(origin, roots.index)?;
    let roots_key = CellKey::new(
        system::ROOTS.id,
        CellNameRef::raw(&head.commit_id.get().to_be_bytes()),
    );
    if origin.get(tables::CELL, &roots_key.encode())?.is_some() {
        return Err(BranchError::RootsCellExists);
    }
    let roots_value = CellValueRef::new(
        CellType::Bytes,
        &[head.state_root, head.index_root].concat(),
        false,
    )?
    .into();
    // The engine's system write is last and authoritative if a low-level caller
    // staged this key. Record-layer authorization remains outside branch.
    let changes = overlay.changes().chain([CellChange::Put {
        key: roots_key,
        value: roots_value,
    }]);
    let mut tx = Buffered::new(origin);
    let cells_update = cells.apply(&mut tx, commit, cell_root, changes)?;
    let mut postings = Vec::new();
    for change in &cells_update.changed_cells {
        let before = term(&change.key, change.before.as_ref())?;
        let after = term(&change.key, change.after.as_ref())?;
        if before == after {
            continue;
        }
        let record_id = change.key.record_id();
        if let Some(term) = before {
            postings.push(PostingChange::Remove { term, record_id });
        }
        if let Some(term) = after {
            postings.push(PostingChange::Add { term, record_id });
        }
    }
    let index_update = index.apply(&mut tx, commit, index_root, postings)?;
    Ok(SealedCommit {
        commit_id,
        state_root: cells_update.root.hash(hasher),
        index_root: index_update.root.hash(hasher),
        cells: cells_update,
        index: index_update,
        writes: tx.into_writes(),
    })
}

fn term(key: &CellKey, value: Option<&CellValue>) -> Result<Option<IndexTerm>> {
    let Some(value) = value.filter(|value| value.is_indexable()) else {
        return Ok(None);
    };
    // Binary reserved field names never reach UTF-8/name validation.
    Ok(Some(IndexTerm::from_cell_name(
        key.name(),
        value.as_view(),
    )?))
}
