use golemdb_storage::{ReadCursor, WriteTransaction};

use crate::{CHANGESET_PREFIX_LEN, HistoryError, HistoryTables, Result};

struct Change {
    key: Vec<u8>,
    commit: u64,
}

impl Change {
    fn item(&self) -> &[u8] {
        &self.key[CHANGESET_PREFIX_LEN..]
    }
}

impl HistoryTables {
    /// Scan up to `max_rows` changesets through `cutoff` inclusive. For a
    /// nonempty batch, the caller decides whether its count and oldest commit
    /// trigger cleanup. Return the highest commit actually removed.
    /// Abort the enclosing writer on error, including after a partial batch.
    pub fn prune_batch(
        &self,
        tx: &mut impl WriteTransaction,
        cutoff: u64,
        max_rows: usize,
        should_prune: impl FnOnce(usize, u64) -> bool,
    ) -> Result<Option<u64>> {
        let mut changes = Vec::new();
        let mut cursor = tx.cursor(self.changeset, b"")?;
        while changes.len() < max_rows {
            let change = cursor.with_next(|row| -> Result<_> {
                let Some((key, _)) = row else { return Ok(None) };
                let prefix =
                    key.first_chunk()
                        .ok_or_else(|| HistoryError::InvalidChangesetKey {
                            table: self.changeset,
                            key: key.to_vec(),
                        })?;
                let commit = u64::from_be_bytes(*prefix);
                Ok((commit <= cutoff).then(|| Change {
                    key: key.to_vec(),
                    commit,
                }))
            })??;
            let Some(change) = change else { break };
            changes.push(change);
        }
        drop(cursor);
        let (Some(first), Some(last)) = (changes.first(), changes.last()) else {
            return Ok(None);
        };
        if !should_prune(changes.len(), first.commit) {
            return Ok(None);
        }

        // Sort references only. The owned keys stay in changeset cursor order.
        let mut by_item: Vec<_> = changes.iter().collect();
        by_item.sort_unstable_by_key(|change| change.item());
        for group in by_item.chunk_by(|a, b| a.item() == b.item()) {
            let item = group[0].item();
            let mut commits = self.read_commits(tx, item)?;
            for change in group {
                if !commits.remove(change.commit) {
                    return Err(HistoryError::MissingCommit {
                        table: self.history,
                        key: item.to_vec(),
                        commit: change.commit,
                    });
                }
            }
            if commits.is_empty() {
                tx.delete(self.history, item)?;
            } else {
                let bytes = commits
                    .encode()
                    .map_err(|source| HistoryError::InvalidCommits {
                        table: self.history,
                        key: item.to_vec(),
                        source,
                    })?;
                tx.put(self.history, item, &bytes)?;
            }
        }
        for change in &changes {
            if !tx.delete(self.changeset, &change.key)? {
                return Err(HistoryError::MissingPreimage {
                    table: self.changeset,
                    key: change.item().to_vec(),
                    commit: change.commit,
                });
            }
        }
        Ok(Some(last.commit))
    }
}
