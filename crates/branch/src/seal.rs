use golemdb_cells::{
    CellChange, CellKey, CellNameRef, CellType, CellValue, CellValueRef, Cells, CellsUpdate,
    reserved, system, tables,
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
    // New trie rows are keyed by this commit.
    let commit = commit_id.get();
    let cells = Cells::new(hasher);
    let index = Index::new(hasher);
    // Validate against the committed origin, before applying ANY staged cell.
    let roots = head_roots(origin, &head, hasher)?;
    cells.validate_root(origin, roots.state)?;
    index.validate_root(origin, roots.index)?;
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
    let cells_update = cells.apply(&mut tx, commit, roots.state, changes)?;
    let mut postings = Vec::new();
    for change in &cells_update.changed_cells {
        let record_id = change.key.record_id();
        // `#is_live` is engine-maintained: the attribute `true`, on user records only
        // (design §3). Any other value, a field `true` included, would silently move the
        // record in or out of the index's live set, so it is refused as a broken
        // invariant. Removing the cell is the record's delete, and always allowed.
        if change.key.name() == reserved::IS_LIVE
            && change.after.as_ref().is_some_and(|after| {
                *after != CellValue::live() || record_id < system::FIRST_USER_RECORD_ID
            })
        {
            return Err(BranchError::InvalidLiveCell { record_id });
        }
        let before = term(&change.key, change.before.as_ref())?;
        let after = term(&change.key, change.after.as_ref())?;
        if before == after {
            continue;
        }
        if let Some(term) = before {
            postings.push(PostingChange::Remove { term, record_id });
        }
        if let Some(term) = after {
            postings.push(PostingChange::Add { term, record_id });
        }
    }
    let index_update = index.apply(&mut tx, commit, roots.index, postings)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Branches, publication_tests::seed};
    use golemdb_cells::{CellKind, system::FIRST_USER_RECORD_ID};
    use golemdb_merkle::Keccak256Hasher;
    use golemdb_storage::MemoryStore;
    use std::sync::Arc;

    fn live_key(record_id: u64) -> CellKey {
        CellKey::new(record_id, reserved::IS_LIVE)
    }

    fn attribute(value: CellValue) -> CellValue {
        value
            .with_kind(CellKind::Attribute)
            .expect("bool and str values can be attributes")
    }

    fn branches() -> Branches<MemoryStore, Keccak256Hasher> {
        let db = MemoryStore::new();
        seed(&db, &Keccak256Hasher, 0, &[]);
        Branches::new(db, Keccak256Hasher, None, None).expect("the head is seeded")
    }

    /// Seal a branch over the head that applies `edit`, the branch still open.
    fn seal_after(
        branches: &Branches<MemoryStore, Keccak256Hasher>,
        edit: impl FnOnce(&mut crate::CellWrite<'_, <MemoryStore as golemdb_storage::Store>::Read<'_>>),
    ) -> (crate::BranchId, Result<Arc<SealedCommit>>) {
        let branch = branches.begin().expect("the head is readable");
        branches
            .write(branch, |cells| {
                edit(cells);
                Ok::<_, ()>(())
            })
            .expect("the branch is open");
        (branch, branches.seal(branch))
    }

    #[test]
    fn the_live_marker_on_a_user_record_joins_the_live_term() {
        let branches = branches();
        let (_, sealed) = seal_after(&branches, |cells| {
            cells.put(live_key(FIRST_USER_RECORD_ID), CellValue::live());
        });
        let sealed = sealed.expect("a valid #is_live seals");
        assert_eq!(sealed.index.changed_terms, [IndexTerm::live()]);
    }

    #[test]
    fn any_other_live_value_is_refused_at_seal() {
        for bad in [
            // The right value as a field would silently leave the live set.
            CellValue::from_bool(true),
            attribute(CellValue::from_bool(false)),
            CellValue::from_bool(false),
            attribute(CellValue::from_str("true")),
        ] {
            let branches = branches();
            let (branch, sealed) = seal_after(&branches, |cells| {
                cells.put(live_key(FIRST_USER_RECORD_ID), bad.clone());
            });
            assert!(
                matches!(
                    sealed,
                    Err(BranchError::InvalidLiveCell {
                        record_id: FIRST_USER_RECORD_ID
                    })
                ),
                "{bad:?}: {sealed:?}"
            );
            // Refusal is a failed seal like any other: the branch stays open.
            assert!(!branches.branch_info(branch).expect("open").sealed);
        }
    }

    #[test]
    fn the_live_marker_on_an_engine_record_is_refused_at_seal() {
        for record_id in [
            system::PARAMS.id,
            system::FIRST_ADMIN_RECORD_ID,
            FIRST_USER_RECORD_ID - 1,
        ] {
            let branches = branches();
            let (_, sealed) = seal_after(&branches, |cells| {
                cells.put(live_key(record_id), CellValue::live());
            });
            assert!(
                matches!(
                    sealed,
                    Err(BranchError::InvalidLiveCell { record_id: refused }) if refused == record_id
                ),
                "record {record_id}: {sealed:?}"
            );
        }
    }

    #[test]
    fn overwriting_a_live_marker_is_refused_and_removing_it_leaves_the_live_term() {
        let branches = branches();
        let (created, sealed) = seal_after(&branches, |cells| {
            cells.put(live_key(FIRST_USER_RECORD_ID), CellValue::live());
        });
        sealed.expect("a valid #is_live seals");
        branches.commit(created).expect("the head has not moved");

        let (_, sealed) = seal_after(&branches, |cells| {
            cells.put(live_key(FIRST_USER_RECORD_ID), CellValue::from_bool(true));
        });
        assert!(matches!(
            sealed,
            Err(BranchError::InvalidLiveCell {
                record_id: FIRST_USER_RECORD_ID
            })
        ));

        let (_, sealed) = seal_after(&branches, |cells| {
            cells.delete(live_key(FIRST_USER_RECORD_ID));
        });
        let sealed = sealed.expect("removing #is_live is a record's delete");
        assert_eq!(sealed.index.changed_terms, [IndexTerm::live()]);
    }

    /// A refused `#is_live` is discarded with its frame, like any write that
    /// fails at seal; the rest of the branch then seals.
    #[test]
    fn a_refused_live_marker_rolls_back_with_its_frame() {
        let branches = branches();
        let branch = branches.begin().expect("the head is readable");
        branches
            .write(branch, |cells| {
                cells.put(live_key(FIRST_USER_RECORD_ID), CellValue::live());
                Ok::<_, ()>(())
            })
            .expect("the branch is open");
        branches.checkpoint(branch).expect("the branch is open");
        branches
            .write(branch, |cells| {
                cells.put(
                    live_key(FIRST_USER_RECORD_ID + 1),
                    CellValue::from_bool(true),
                );
                Ok::<_, ()>(())
            })
            .expect("the branch is open");
        assert!(matches!(
            branches.seal(branch),
            Err(BranchError::InvalidLiveCell { record_id }) if record_id == FIRST_USER_RECORD_ID + 1
        ));
        branches.rollback(branch).expect("a frame remains");
        let sealed = branches.seal(branch).expect("the valid frame seals");
        assert_eq!(sealed.index.changed_terms, [IndexTerm::live()]);
    }
}
