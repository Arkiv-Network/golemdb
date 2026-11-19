use golemdb_storage::{ReadTransaction, StorageError, Table, WriteTransaction};

use crate::{CommitSet, HistoryError, ReadOperation, Result};

/// Whether to use the owner's live value or a recorded pre-image.
/// `B` is `Vec<u8>` for owned reads and `&[u8]` inside borrowed callbacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueSource<B = Vec<u8>> {
    /// No later change exists; read the live value in the same transaction.
    Live,
    /// The selected change records a value (`Some`) or prior absence (`None`).
    Preimage(Option<B>),
}

/// Tables owned and named by a domain crate. The owner must supply distinct tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryTables {
    pub history: Table,
    pub changeset: Table,
}

/// Bytes prepended to an item key in a changeset: the modifying commit as `u64_be`.
pub const CHANGESET_PREFIX_LEN: usize = size_of::<u64>();

/// Big-endian modifying commit, followed by the complete opaque item key.
pub fn changeset_key(commit: u64, item: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(CHANGESET_PREFIX_LEN + item.len());
    key.extend_from_slice(&commit.to_be_bytes());
    key.extend_from_slice(item);
    key
}

impl HistoryTables {
    pub const fn new(history: Table, changeset: Table) -> Self {
        Self { history, changeset }
    }

    /// Read/decode the commit set without work callbacks. Missing history is an
    /// empty set; malformed stored history is an error.
    pub fn read_commits(&self, tx: &impl ReadTransaction, item: &[u8]) -> Result<CommitSet> {
        self.read_commits_with_callbacks(tx, item, &mut (), |_, _| Ok(()))
    }

    /// Authorize lookup before storage access, then processing before decoding the stored bytes.
    /// Missing history is an empty set; malformed stored history is an error.
    pub fn read_commits_with_callbacks<C, E: From<HistoryError>>(
        &self,
        tx: &impl ReadTransaction,
        item: &[u8],
        context: &mut C,
        mut before: impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
    ) -> std::result::Result<CommitSet, E> {
        before(context, ReadOperation::Lookup(self.history))?;
        tx.with_value(self.history, item, |bytes| match bytes {
            None => Ok(CommitSet::default()),
            Some(bytes) => {
                before(
                    context,
                    ReadOperation::CommitSet {
                        encoded_bytes: bytes.len() as u64,
                    },
                )?;
                CommitSet::decode(bytes).map_err(|source| {
                    HistoryError::InvalidCommits {
                        table: self.history,
                        key: item.to_vec(),
                        source,
                    }
                    .into()
                })
            }
        })
        .map_err(HistoryError::from)?
    }

    /// Resolve owned pre-image bytes. Coverage and target/head validation belong
    /// to the caller. `Live` must be read from this same transaction.
    pub fn resolve(
        &self,
        tx: &impl ReadTransaction,
        item: &[u8],
        target: u64,
    ) -> Result<ValueSource> {
        self.with_resolved(tx, item, target, |source| match source {
            ValueSource::Live => ValueSource::Live,
            ValueSource::Preimage(bytes) => ValueSource::Preimage(bytes.map(<[u8]>::to_vec)),
        })
    }

    /// Inspect the selected source without work callbacks. Resolution and
    /// storage errors are handled before calling `inspect`.
    pub fn with_resolved<T>(
        &self,
        tx: &impl ReadTransaction,
        item: &[u8],
        target: u64,
        inspect: impl FnOnce(ValueSource<&[u8]>) -> T,
    ) -> Result<T> {
        self.with_resolved_with_callbacks(
            tx,
            item,
            target,
            &mut (),
            |_, _| Ok(()),
            |_, source| Ok(inspect(source)),
        )
    }

    /// Resolve through upfront work checkpoints. The commit-set package includes
    /// successor seeking; a missing history row needs no processing callback.
    pub fn with_resolved_with_callbacks<C, T, E: From<HistoryError>>(
        &self,
        tx: &impl ReadTransaction,
        item: &[u8],
        target: u64,
        context: &mut C,
        mut before: impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
        inspect: impl FnOnce(&mut C, ValueSource<&[u8]>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        match self
            .read_commits_with_callbacks(tx, item, context, &mut before)?
            .first_after(target)
        {
            None => inspect(context, ValueSource::Live),
            Some(commit) => self.with_preimage_with_callbacks(
                tx,
                item,
                commit,
                context,
                before,
                |context, bytes| inspect(context, ValueSource::Preimage(bytes)),
            ),
        }
    }

    /// Inspect a selected pre-image without rereading history. A scan can decode
    /// its row with `CommitSet::decode`, select `first_after`, then call here.
    /// A stored empty row yields `None`; a missing row is an error and does not
    /// invoke the callback.
    pub fn with_preimage<T>(
        &self,
        tx: &impl ReadTransaction,
        item: &[u8],
        commit: u64,
        inspect: impl FnOnce(Option<&[u8]>) -> T,
    ) -> Result<T> {
        self.with_preimage_with_callbacks(
            tx,
            item,
            commit,
            &mut (),
            |_, _| Ok(()),
            |_, bytes| Ok(inspect(bytes)),
        )
    }

    /// Inspect a known pre-image after authorizing its lookup; does not repeat
    /// history discovery or commit-set processing.
    pub fn with_preimage_with_callbacks<C, T, E: From<HistoryError>>(
        &self,
        tx: &impl ReadTransaction,
        item: &[u8],
        commit: u64,
        context: &mut C,
        mut before: impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
        inspect: impl FnOnce(&mut C, Option<&[u8]>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        before(context, ReadOperation::Lookup(self.changeset))?;
        tx.with_value(self.changeset, &changeset_key(commit, item), |bytes| {
            let bytes = bytes.ok_or_else(|| HistoryError::MissingPreimage {
                table: self.changeset,
                key: item.to_vec(),
                commit,
            })?;
            inspect(context, (!bytes.is_empty()).then_some(bytes))
        })
        .map_err(HistoryError::from)?
    }

    /// Record one net item change, with its value immediately before commit.
    /// Commits must increase for this item. The owner enforces global commit
    /// ordering and coverage, including after pruning has emptied a history.
    /// Duplicate pre-images are never overwritten, even if history is missing.
    /// `None` records prior absence as an empty row. `Some` must be nonempty.
    /// Abort the enclosing transaction after any error: this is two writes.
    pub fn record(
        &self,
        tx: &mut impl WriteTransaction,
        item: &[u8],
        commit: u64,
        preimage: Option<&[u8]>,
    ) -> Result<()> {
        if preimage.is_some_and(<[u8]>::is_empty) {
            return Err(HistoryError::EmptyPreimage);
        }
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
        tx.insert(
            self.changeset,
            &changeset_key(commit, item),
            preimage.unwrap_or_default(),
        )
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
