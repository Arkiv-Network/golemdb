use golemdb_cells::{
    CellChange, CellKey, CellNameRef, CellType, CellValueRef, Cells, CellsUpdate, system, tables,
};
use golemdb_index::{Index, IndexUpdate};
use golemdb_merkle::{Hash, HashProvider};
use golemdb_storage::ReadTransaction;

use crate::{
    BranchError, CommitId, Result,
    buffer::{Buffered, Writes},
    head::{Head, read_head_state, read_retention, write_head},
    overlay::CellOverlay,
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
    // Replayed by commit inside a writer whose head still equals `origin`.
    pub(crate) writes: Writes,
    pub(crate) origin: Head,
}

pub(crate) fn compute(
    origin: &impl ReadTransaction,
    overlay: &CellOverlay,
    hasher: &impl HashProvider,
) -> Result<SealedCommit> {
    let head = read_head_state(origin)?;
    let retention = read_retention(origin)?;
    let commit_id = head
        .commit_id
        .get()
        .checked_add(1)
        .map(crate::CommitId::new)
        .ok_or(BranchError::CommitNumberExhausted)?;
    let cells = Cells::new(hasher);
    let index = Index::new(hasher);
    // Reopen against the committed origin, before applying ANY staged cell.
    let cell_root = cells.reopen(origin, head.state_root)?;
    let index_root = index.reopen(origin, head.index_root)?;
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
    let (cells_update, index_update) =
        crate::state::apply(&mut tx, hasher, cell_root, index_root, changes)?;
    crate::gc::update(
        &mut tx,
        retention,
        cell_root,
        index_root,
        &cells_update,
        &index_update,
    )?;
    let state_root = cells_update.root.hash(hasher);
    let index_root = index_update.root.hash(hasher);
    write_head(
        &mut tx,
        &Head {
            commit_id,
            state_root,
            index_root,
        },
    )?;
    Ok(SealedCommit {
        commit_id,
        state_root,
        index_root,
        cells: cells_update,
        index: index_update,
        writes: tx.into_writes(),
        origin: head,
    })
}
