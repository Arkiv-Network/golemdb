use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use golemdb_storage::WriteTransaction;

use crate::{
    BranchError, CommitId, Result, SealedCommit,
    head::{Head, read_head, write_head},
    roots::{Roots, write_roots},
};

/// The writer has serialized with other publishers before this guard runs.
/// Any error drops the transaction, including all partially replayed rows.
pub(crate) fn persist(
    mut tx: impl WriteTransaction,
    origin: CommitId,
    sealed: &SealedCommit,
) -> Result<CommitId> {
    // Drop a failed writer outside unwinding so backend mutexes are not poisoned
    // by a panic while reading head or replaying rows. Nothing has been published.
    match catch_unwind(AssertUnwindSafe(|| stage(&mut tx, origin, sealed))) {
        Ok(result) => result?,
        Err(panic) => {
            drop(tx);
            resume_unwind(panic);
        }
    }
    tx.commit()?;
    Ok(sealed.commit_id)
}

fn stage(tx: &mut impl WriteTransaction, origin: CommitId, sealed: &SealedCommit) -> Result<()> {
    if read_head(tx)? != origin {
        return Err(BranchError::Conflict);
    }
    for (table, rows) in &sealed.writes {
        // `rows` is in key order, and this commit's version keys sort last.
        let versioned = golemdb_cells::tables::VERSIONED.contains(table)
            || golemdb_index::tables::VERSIONED.contains(table);
        for (key, value) in rows {
            match value {
                Some(value) if versioned => tx.append(*table, key, value)?,
                Some(value) => tx.put(*table, key, value)?,
                None => {
                    tx.delete(*table, key)?;
                }
            }
        }
    }
    // History and change-set tables remain deferred. Allocator/binding changes
    // already staged as cells participate in the same sealed write set.
    let roots = Roots {
        state: sealed.cells.root,
        index: sealed.index.root,
    };
    write_roots(tx, sealed.commit_id, &roots)?;
    write_head(
        tx,
        &Head {
            commit_id: sealed.commit_id,
            state_root: sealed.state_root,
            index_root: sealed.index_root,
        },
    )
}
