use golemdb_storage::{ReadTransaction, StorageError, Table, WriteTransaction};

use crate::{CommitSet, HistoryError, Result};

/// Whether to use the owner's live value or a recorded pre-image.
/// `B` is `Vec<u8>` for owned reads and `&[u8]` inside borrowed callbacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueSource<B = Vec<u8>> {
    Live,
    Preimage(B),
}

/// The two distinct tables owned and named by a domain crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryTables {
    pub history: Table,
    pub changeset: Table,
}

/// Big-endian modifying commit, followed by the complete opaque item key.
pub fn changeset_key(commit: u64, item: &[u8]) -> Vec<u8> {
    [&commit.to_be_bytes()[..], item].concat()
}

impl HistoryTables {
    pub const fn new(history: Table, changeset: Table) -> Self {
        Self { history, changeset }
    }

    fn validate(&self) -> Result<()> {
        self.history.validate()?;
        self.changeset.validate()?;
        if self.history == self.changeset {
            return Err(HistoryError::AliasedTables(self.history));
        }
        Ok(())
    }

    /// Read/decode the commit set without copying the stored history bytes.
    /// Missing history is an empty set; malformed stored history is an error.
    pub fn read_commits(&self, tx: &impl ReadTransaction, item: &[u8]) -> Result<CommitSet> {
        self.validate()?;
        tx.with_value(self.history, item, |bytes| match bytes {
            None => Ok(CommitSet::default()),
            Some(bytes) => {
                CommitSet::decode(bytes).map_err(|source| HistoryError::InvalidCommits {
                    table: self.history,
                    key: item.to_vec(),
                    source,
                })
            }
        })?
    }

    /// Resolve owned pre-image bytes. Coverage and target/head validation belong
    /// to the caller. `Live` must be read from this same transaction.
    pub fn resolve(
        &self,
        tx: &impl ReadTransaction,
        item: &[u8],
        target: u64,
    ) -> Result<ValueSource> {
        self.with_resolved(tx, item, target, |resolved| match resolved {
            ValueSource::Live => ValueSource::Live,
            ValueSource::Preimage(bytes) => ValueSource::Preimage(bytes.to_vec()),
        })
    }

    /// Inspect the selected pre-image before allocation/copying. The callback is
    /// not invoked on storage errors, malformed history or a missing pre-image.
    pub fn with_resolved<T>(
        &self,
        tx: &impl ReadTransaction,
        item: &[u8],
        target: u64,
        inspect: impl FnOnce(ValueSource<&[u8]>) -> T,
    ) -> Result<T> {
        match self.read_commits(tx, item)?.first_after(target) {
            None => Ok(inspect(ValueSource::Live)),
            Some(commit) => self.with_preimage(tx, item, commit, |bytes| {
                inspect(ValueSource::Preimage(bytes))
            }),
        }
    }

    /// Inspect a selected pre-image without rereading history. A scan can decode
    /// its row with `CommitSet::decode`, select `first_after`, then call here.
    /// Empty bytes are passed through; a missing row is never treated as absence.
    pub fn with_preimage<T>(
        &self,
        tx: &impl ReadTransaction,
        item: &[u8],
        commit: u64,
        inspect: impl FnOnce(&[u8]) -> T,
    ) -> Result<T> {
        self.validate()?;
        tx.with_value(self.changeset, &changeset_key(commit, item), |bytes| {
            bytes
                .map(inspect)
                .ok_or_else(|| HistoryError::MissingPreimage {
                    table: self.changeset,
                    key: item.to_vec(),
                    commit,
                })
        })?
    }

    /// Record one net item change, with its value immediately before commit.
    /// Commits must increase for this item. The owner enforces global commit
    /// ordering and coverage, including after pruning has emptied a history.
    /// Duplicate pre-images are never overwritten, even if history is missing.
    /// Abort the enclosing transaction after any error: this is two writes.
    pub fn record(
        &self,
        tx: &mut impl WriteTransaction,
        item: &[u8],
        commit: u64,
        preimage: &[u8],
    ) -> Result<()> {
        let mut commits = self.read_commits(tx, item)?;
        if let Some(previous) = commits.latest()
            && commit <= previous
        {
            return Err(HistoryError::NonIncreasingCommit {
                table: self.history,
                key: item.to_vec(),
                previous,
                commit,
            });
        }
        commits.insert(commit);
        let bytes = commits
            .encode()
            .map_err(|source| HistoryError::InvalidCommits {
                table: self.history,
                key: item.to_vec(),
                source,
            })?;
        tx.insert(self.changeset, &changeset_key(commit, item), preimage)
            .map_err(|error| match error {
                StorageError::AlreadyExists => HistoryError::ExistingPreimage {
                    table: self.changeset,
                    key: item.to_vec(),
                    commit,
                },
                error => HistoryError::Storage(error),
            })?;
        tx.put(self.history, item, &bytes)?;
        Ok(())
    }
}
