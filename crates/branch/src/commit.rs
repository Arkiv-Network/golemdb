use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use golemdb_immutable_data::{ImmutableDataStorageError as DataError, metadata};
use golemdb_merkle::HashProvider;
use golemdb_pruning::Pruner;
use golemdb_storage::{Store, WriteTransaction};

use crate::{
    BranchError, CommitId, Result, SealedCommit,
    head::{Head, advance_head, read_head, read_head_state},
    immutable::{ImmutableSegments, ImmutableStaging},
    manager::ImmutableWriter,
    roots::Roots,
};

/// The writer has serialized with other publishers before this guard runs.
/// Any error drops the transaction, including all partially replayed rows.
pub(crate) fn persist(
    mut tx: impl WriteTransaction,
    origin: CommitId,
    sealed: &SealedCommit,
    hasher: &impl HashProvider,
    pruner: Option<&Pruner>,
) -> Result<CommitId> {
    // Drop a failed writer outside unwinding so backend mutexes are not poisoned
    // by a panic while reading head or replaying rows. Nothing has been published.
    match catch_unwind(AssertUnwindSafe(|| {
        stage(&mut tx, origin, sealed, hasher, pruner)
    })) {
        Ok(result) => result?,
        Err(panic) => {
            drop(tx);
            resume_unwind(panic);
        }
    }
    tx.commit()?;
    Ok(sealed.commit_id)
}

pub(crate) fn stage(
    tx: &mut impl WriteTransaction,
    origin: CommitId,
    sealed: &SealedCommit,
    hasher: &impl HashProvider,
    pruner: Option<&Pruner>,
) -> Result<()> {
    // Checked before replaying, so a lost race does no work; `advance_head`
    // checks again with the head it publishes.
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
    if let Some(pruner) = pruner {
        let commit = sealed.commit_id.get();
        let superseded = (sealed.cells.superseded.iter())
            .chain(&sealed.index.superseded)
            .map(|(table, key)| (*table, key.as_slice()));
        pruner.register_tries(tx, commit, superseded)?;
        pruner.sweep_tries(tx, commit)?;
        pruner.prune_commit_keyed_rows(tx, crate::ROOTS, commit)?;
    }
    // Publishes `origin + 1`, the commit seal wrote these rows under.
    advance_head(tx, origin, &roots, hasher)?;
    Ok(())
}

/// Called with the immutable writer lock held through outcome resolution
/// and rollback. Committed readers use their own handles and never take this lock.
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish(
    writer: &mut ImmutableWriter,
    store: &impl Store,
    origin: CommitId,
    sealed: &SealedCommit,
    staged: &ImmutableStaging,
    segments: &ImmutableSegments,
    hasher: &impl HashProvider,
    pruner: Option<&Pruner>,
) -> Result<CommitId> {
    if writer.recovery_required {
        return Err(BranchError::RecoveryRequired);
    }
    let storage = &mut writer.storage;
    let mut tx = store.begin_write()?;
    let before = read_head_state(&tx)?;
    if before.commit_id != origin {
        return Err(BranchError::Conflict);
    }
    writer.recovery_required = true;
    let mut attempted_commit = false;
    let outcome = (|| -> Result<()> {
        crate::commit::stage(&mut tx, origin, sealed, hasher, pruner)?;
        for (id, batch) in staged.batches() {
            let name = segments.by_id(id).schema.name();
            if metadata::end_ordinal_at(&tx, id, origin.get())? != batch.first_ordinal
                || storage.end_ordinal(name)? != batch.first_ordinal
            {
                return Err(DataError::Corrupt(
                    "staged ordinal disagrees with published or physical end",
                )
                .into());
            }
            for (offset, (key, row)) in batch.rows.iter().enumerate() {
                let ordinal = storage.append(name, sealed.commit_id.get(), row)?;
                if ordinal != batch.first_ordinal + offset as u64 {
                    return Err(
                        DataError::Corrupt("physical ordinal changed during publication").into(),
                    );
                }
                if let Some(key) = key {
                    metadata::bind_key(&mut tx, id, *key, ordinal)?;
                }
            }
            metadata::record_commit_range(
                &mut tx,
                id,
                sealed.commit_id.get(),
                batch.first_ordinal..batch.end()?,
            )?;
        }
        if !staged.is_empty() {
            storage.sync()?;
        }
        attempted_commit = true;
        tx.commit()?;
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            writer.recovery_required = false;
            Ok(sealed.commit_id)
        }
        Err(failure) => {
            // Dropping/consuming the first transaction released its writer.
            // Reacquire exclusivity before resolving an uncertain outcome.
            let resolution = (|| -> Result<bool> {
                let tx = store.begin_write()?;
                let actual = read_head_state(&tx)?;
                let expected = Head {
                    commit_id: sealed.commit_id,
                    state_root: sealed.state_root,
                    index_root: sealed.index_root,
                };
                if attempted_commit && actual == expected {
                    for (id, batch) in staged.batches() {
                        if metadata::commit_range(&tx, id, sealed.commit_id.get())?
                            != Some(batch.first_ordinal..batch.end()?)
                        {
                            return Err(BranchError::RecoveryRequired);
                        }
                        for (offset, (key, _)) in batch.rows.iter().enumerate() {
                            if let Some(key) = key
                                && metadata::ordinal_for_key(&tx, id, *key)?
                                    != Some(batch.first_ordinal + offset as u64)
                            {
                                return Err(BranchError::RecoveryRequired);
                            }
                        }
                    }
                    Ok(true)
                } else if actual == before {
                    storage.recover(&tx, before.commit_id.get())?;
                    Ok(false)
                } else {
                    Err(BranchError::RecoveryRequired)
                }
            })();
            match resolution {
                Ok(committed) => {
                    writer.recovery_required = false;
                    if committed {
                        Ok(sealed.commit_id)
                    } else {
                        Err(failure)
                    }
                }
                Err(recovery) => Err(BranchError::PublicationRecovery {
                    publication: Box::new(failure),
                    recovery: Box::new(recovery),
                }),
            }
        }
    }
}
