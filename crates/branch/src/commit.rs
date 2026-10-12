use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use golemdb_storage::WriteTransaction;

use crate::{BranchError, CommitId, Result, SealedCommit, buffer::Writes, head::read_head_state};

/// The writer has serialized with other publishers before this guard runs.
/// Any error drops the transaction, including all partially replayed rows.
pub(crate) fn persist(tx: impl WriteTransaction, sealed: &SealedCommit) -> Result<CommitId> {
    write_atomically(tx, |tx| {
        if read_head_state(tx)? != sealed.origin {
            return Err(BranchError::HandleInvalid);
        }
        // Cells, indexes, GC counts/deletions and head were staged by seal.
        // History and change-set tables remain deferred.
        replay(tx, &sealed.writes)?;
        Ok(sealed.commit_id)
    })
}

/// Stage and commit one writer. Drop a failed writer outside unwinding so
/// backend mutexes are not poisoned by a panic while staging rows.
pub(crate) fn write_atomically<W: WriteTransaction, T>(
    mut tx: W,
    stage: impl FnOnce(&mut W) -> Result<T>,
) -> Result<T> {
    let value = match catch_unwind(AssertUnwindSafe(|| stage(&mut tx))) {
        Ok(result) => result?,
        Err(panic) => {
            drop(tx);
            resume_unwind(panic);
        }
    };
    tx.commit()?;
    Ok(value)
}

pub(crate) fn replay(tx: &mut impl WriteTransaction, writes: &Writes) -> Result<()> {
    for (table, rows) in writes {
        for (key, value) in rows {
            match value {
                Some(value) => tx.put(*table, key, value)?,
                None => {
                    tx.delete(*table, key)?;
                }
            }
        }
    }
    Ok(())
}
