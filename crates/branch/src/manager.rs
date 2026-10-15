use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use golemdb_immutable_data::{
    ImmutableDataConfig, ImmutableDataKey, ImmutableDataReader, ImmutableDataRow,
    ImmutableDataStorage,
};
use golemdb_merkle::HashProvider;
use golemdb_storage::{ReadTransaction, Store};

use crate::{
    BranchError, BranchId, BranchInfo, CellRead, CellWrite, CommitId, OperationError, Result,
    SealedCommit,
    head::read_head,
    immutable::{ImmutableSegments, ImmutableStaging},
    overlay::CellOverlay,
};

// A process-wide counter prevents handle aliasing between independent managers,
// including a manager reopened over the same head after the old one was dropped.
static NEXT_BRANCH_ID: AtomicU64 = AtomicU64::new(0);

struct BranchState {
    commit_id: CommitId,
    overlay: CellOverlay,
    immutable_data: ImmutableStaging,
    sealed: Option<Arc<SealedCommit>>,
}

impl BranchState {
    fn require_open(&self) -> Result<()> {
        if self.sealed.is_some() {
            Err(BranchError::Sealed)
        } else {
            Ok(())
        }
    }

    fn seal(
        &mut self,
        origin: &impl ReadTransaction,
        hasher: &impl HashProvider,
    ) -> Result<Arc<SealedCommit>> {
        if let Some(sealed) = &self.sealed {
            return Ok(Arc::clone(sealed));
        }
        let sealed = Arc::new(crate::seal::compute(origin, &self.overlay, hasher)?);
        self.sealed = Some(Arc::clone(&sealed));
        Ok(sealed)
    }
}

type Entry = Arc<Mutex<Option<BranchState>>>;

/// Immutable-data configuration supplied when creating a branch manager.
/// Construct with [`Self::files`] for persistent files or [`Self::memory`] for
/// in-memory storage. Storage is opened and recovered by [`Branches::new`].
#[derive(Clone, Debug)]
pub struct ImmutableDataOptions {
    config: ImmutableDataConfig,
    genesis_id: [u8; 32],
    directory: Option<PathBuf>,
}

impl ImmutableDataOptions {
    /// Store immutable rows in `directory`, using the supplied genesis schema and identity.
    pub fn files(
        config: ImmutableDataConfig,
        genesis_id: [u8; 32],
        directory: impl AsRef<Path>,
    ) -> Self {
        Self {
            config,
            genesis_id,
            directory: Some(directory.as_ref().to_path_buf()),
        }
    }

    /// Store immutable rows in memory, using the supplied genesis schema and identity.
    pub fn memory(config: ImmutableDataConfig, genesis_id: [u8; 32]) -> Self {
        Self {
            config,
            genesis_id,
            directory: None,
        }
    }
}

/// Persistent immutable storage, locked through commit and any file recovery.
pub(crate) struct ImmutableWriter {
    pub storage: ImmutableDataStorage,
    // Set before file changes; cleared only on a resolved outcome and cleanup.
    pub recovery_required: bool,
}

struct Inner<S, H> {
    hasher: H,
    store: S,
    branches: Mutex<BTreeMap<BranchId, Entry>>,
    immutable_segments: ImmutableSegments,
    immutable_writer: Option<Mutex<ImmutableWriter>>,
    immutable_reader: Option<ImmutableDataReader>,
}

/// Head-only lifecycle with guarded cell state and independent immutable batches.
///
/// Clones share the registry and store. Each branch has its own lock, held
/// for an entire operation; independent branches can execute concurrently when
/// the store supports sharing between threads. No storage snapshot is kept
/// between calls. Every call on a branch ID checks the head in a fresh snapshot
/// acquired *after* locking the branch, then uses that snapshot for cell reads.
/// An in-flight operation may finish over its snapshot if head moves meanwhile;
/// the next call rejects the stale ID. Stale branches are reclaimed when
/// accessed; dropping the last manager clone releases all remaining branches.
///
/// Callbacks should use their supplied view, not re-enter branch operations:
/// the current branch lock is already held and nested calls can deadlock. They
/// can return owned results, but cannot retain views or borrowed scans.
///
/// This increment does not initialize genesis, validate format/hash settings,
/// authenticate entire tries, maintain history, or rewind the store. The supplied hash
/// provider must match the deployment and is shared by all branch seals. External writers
/// must publish cells and a strictly increasing head atomically; changing cells
/// under an unchanged head or rewinding it violates this manager's contract.
/// When physical immutable storage is attached, only this manager and its clones
/// may publish head or immutable metadata; independent publishers are unsupported.
///
/// # Callback panics
///
/// An unwinding panic escaping a [`read`](Self::read) or [`write`](Self::write)
/// callback is resumed with its original payload after releasing the branch
/// lock. It is not converted into an [`OperationError`] and does not poison
/// that lock. A write first restores its pre-call overlay, version, and checkpoint
/// state; earlier successful operations remain intact. A read leaves them untouched.
/// If the caller catches the panic, subsequent operations can use the branch,
/// subject to normal head validation.
///
/// This recovery requires unwinding and successful cleanup; it does not apply
/// to `panic = "abort"` or a second panic during unwinding. Only branch state is
/// restored, not side effects performed by the callback outside its cell view.
/// This is not a general guarantee for backend panics: storage snapshot creation
/// and head validation happen before the callback's panic guard, so a panic there
/// can poison the branch lock. See [`commit`](Self::commit) for publication limits.
pub struct Branches<S, H> {
    inner: Arc<Inner<S, H>>,
}

impl<S, H> Clone for Branches<S, H> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<S: Store, H: HashProvider> Branches<S, H> {
    /// Open a manager over an initialized head. `None` disables immutable data.
    /// With options, opens immutable storage and recovers it to the durable head
    /// before returning. Supply matching genesis declarations and identity.
    /// Only this manager and its clones may publish head or immutable metadata
    /// while it owns immutable storage. The store must provide durable commit
    /// and authoritative snapshots after commit errors.
    /// A missing, malformed or unreadable head is rejected before file access.
    pub fn new(store: S, hasher: H, immutable_data: Option<ImmutableDataOptions>) -> Result<Self> {
        {
            let tx = store.begin_read()?;
            read_head(&tx)?;
        }
        let mut inner = Inner {
            store,
            hasher,
            branches: Mutex::new(BTreeMap::new()),
            immutable_segments: ImmutableSegments::default(),
            immutable_writer: None,
            immutable_reader: None,
        };
        if let Some(options) = immutable_data {
            let mut storage = match options.directory {
                Some(directory) => {
                    ImmutableDataStorage::open(options.config, options.genesis_id, directory)?
                }
                None => ImmutableDataStorage::in_memory(options.config, options.genesis_id)?,
            };
            let segments = ImmutableSegments::new(storage.config().clone())?;
            // Exclude publishers while recovering against an authoritative snapshot.
            let tx = inner.store.begin_write()?;
            storage.recover(&tx, read_head(&tx)?.get())?;
            drop(tx);
            inner.immutable_reader = Some(storage.reader());
            inner.immutable_segments = segments;
            inner.immutable_writer = Some(Mutex::new(ImmutableWriter {
                storage,
                recovery_required: false,
            }));
        }
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn head(&self) -> Result<CommitId> {
        read_head(&self.inner.store.begin_read()?)
    }

    /// The store shared by this manager. Committed readers can open a
    /// snapshot directly; no branch registration or overlay is needed.
    /// Direct writers must obey the publication contract documented on Branches.
    pub fn store(&self) -> &S {
        &self.inner.store
    }

    /// The hash provider shared by this manager. Committed readers use it to
    /// decode index roots in their own snapshot.
    pub fn hasher(&self) -> &H {
        &self.inner.hasher
    }

    /// Open a new independent overlay and its first frame over the current head.
    pub fn begin(&self) -> Result<BranchId> {
        let state = BranchState {
            commit_id: self.head()?,
            overlay: CellOverlay::new(),
            immutable_data: ImmutableStaging::default(),
            sealed: None,
        };
        let branch_id = allocate_branch_id(&NEXT_BRANCH_ID)?;
        self.inner
            .branches
            .lock()
            .map_err(|_| BranchError::Poisoned("registry"))?
            .insert(branch_id, Arc::new(Mutex::new(Some(state))));
        Ok(branch_id)
    }

    /// Read metadata under the branch lock and the usual head validation.
    /// Version counts retained undo entries; sealing does not add journal entries.
    pub fn branch_info(&self, branch_id: BranchId) -> Result<BranchInfo> {
        self.with_branch(branch_id, |state, _| BranchInfo {
            commit_id: state.commit_id,
            branch_id,
            version: state.overlay.version(),
            sealed: state.sealed.is_some(),
        })
    }

    /// Read cells from one validated branch snapshot. Callback errors are kept
    /// separate from errors admitting the operation (such as a stale ID).
    /// Sealed branches remain readable. The `#roots` cell added by sealing is
    /// part of publication and becomes visible only after commit.
    ///
    /// # Panics
    ///
    /// A panic escaping the callback leaves branch state unchanged and is resumed
    /// after releasing the branch lock without poisoning it. See [`Branches`]
    /// for the callback panic contract and its limits.
    pub fn read<T, E>(
        &self,
        branch_id: BranchId,
        operation: impl FnOnce(&CellRead<'_, S::Read<'_>>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, OperationError<E>> {
        self.with_branch(branch_id, |state, origin| {
            operation(&CellRead::new(&state.overlay, origin)).map_err(OperationError::Operation)
        })
        .map_err(OperationError::Branch)?
    }

    /// Atomically apply one cell operation group after validating its ID.
    /// Returning `Ok` retains all writes. Returning `Err` restores the overlay,
    /// version, and checkpoint state from before this call and returns
    /// [`OperationError::Operation`]. Earlier successful operations are preserved;
    /// no checkpoint is needed to make this callback atomic.
    ///
    /// # Panics
    ///
    /// A panic escaping the callback performs the same restoration as `Err`, then
    /// resumes the original panic after releasing the branch lock without poisoning
    /// it. It is not returned as an error. If the callback catches its own panic
    /// and returns `Ok`, its writes are retained. See [`Branches`] for recovery limits.
    pub fn write<T, E>(
        &self,
        branch_id: BranchId,
        operation: impl FnOnce(&mut CellWrite<'_, S::Read<'_>>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, OperationError<E>> {
        self.with_branch(branch_id, |state, origin| {
            state.require_open().map_err(OperationError::Branch)?;
            state
                .overlay
                .write(origin, operation)
                .map_err(OperationError::Operation)
        })
        .map_err(OperationError::Branch)?
    }

    pub fn checkpoint(&self, branch_id: BranchId) -> Result<()> {
        self.with_branch(branch_id, |state, _| {
            state.require_open()?;
            state.overlay.checkpoint();
            Ok(())
        })?
    }

    /// Stage a complete row before or after seal, without writing files or KV.
    /// Ordinals start at the published segment end and remain provisional. Keys
    /// are unique against published bindings and this branch's own pending rows.
    /// This operation never changes cell checkpoints or BranchInfo::version.
    pub fn immutable_data_append(
        &self,
        branch_id: BranchId,
        segment: &str,
        key: Option<ImmutableDataKey>,
        row: ImmutableDataRow,
    ) -> Result<u64> {
        self.with_branch(branch_id, |state, origin| {
            let segment = self.inner.immutable_segments.get(segment)?;
            state
                .immutable_data
                .append(origin, state.commit_id.get(), segment, key, row)
                .map_err(BranchError::from)
        })?
    }

    /// Remove only this branch's pending tail [from_ordinal, staged_end), before
    /// or after seal. The bound must lie between the origin's published end and
    /// the staged end, inclusive. Removed keys and ordinals become reusable in
    /// this branch. Live cells, their checkpoints and committed rows are untouched.
    pub fn immutable_data_truncate(
        &self,
        branch_id: BranchId,
        segment: &str,
        from_ordinal: u64,
    ) -> Result<()> {
        self.with_branch(branch_id, |state, origin| {
            let segment = self.inner.immutable_segments.get(segment)?;
            state
                .immutable_data
                .truncate(origin, state.commit_id.get(), segment, from_ordinal)
                .map_err(BranchError::from)
        })?
    }

    /// Independent committed reader, or None when immutable data is disabled.
    /// Calls on this handle never acquire the immutable writer lock. The caller supplies
    /// one published store snapshot and the commit number read from that snapshot.
    pub fn immutable_data(&self) -> Option<&ImmutableDataReader> {
        self.inner.immutable_reader.as_ref()
    }

    /// Validate the head, then undo one frame in memory. Only admission reads
    /// storage; the inverse replay itself performs no storage access.
    pub fn rollback(&self, branch_id: BranchId) -> Result<()> {
        self.with_branch(branch_id, |state, _| {
            state.require_open()?;
            state.overlay.rollback()
        })?
    }

    /// Compute cell/index updates and stage physical rows without opening a
    /// store writer. Failure leaves the overlay and checkpoints untouched.
    /// Success freezes cell access. Repeated sealing returns the cached result,
    /// after validating head again; it does not reserve a commit number.
    ///
    /// # Panics
    ///
    /// An unwinding panic during seal computation leaves the branch open, with
    /// its overlay, version, and checkpoints unchanged. The panic is resumed after
    /// releasing the branch lock. The recovery limits on [`Branches`] apply.
    pub fn seal(&self, branch_id: BranchId) -> Result<Arc<SealedCommit>> {
        self.with_branch(branch_id, |state, origin| {
            state.seal(origin, &self.inner.hasher)
        })?
    }

    /// Seal if necessary, then atomically persist the computed rows and head.
    /// Head is checked again inside the storage writer before any row is written.
    /// Success consumes the branch; competitors over the old head become stale.
    /// A failed seal leaves the branch open. A storage failure after sealing
    /// retains the sealed result for retry or discard, subject to head validation.
    /// A live branch whose origin lost the race returns Conflict and is consumed.
    /// Unknown or already invalidated/consumed handles return HandleInvalid.
    /// With physical immutable storage attached, publication appends and syncs
    /// files, then atomically publishes cells, index,
    /// keys, ranges and head. A failed transaction is resolved against durable
    /// metadata before removing unpublished tails. Unresolved outcomes or failed
    /// recovery block further publication until reopen and recovery. Independent
    /// readers can still read intact committed prefixes from published snapshots.
    ///
    /// # Panics
    ///
    /// A panic during seal computation has the same behavior as [`seal`](Self::seal).
    /// An unwinding panic while checking the writer's head or replaying rows drops
    /// the uncommitted transaction before resuming the panic; the sealed result is
    /// retained. If the backend panics inside its consuming transaction `commit`,
    /// the publication outcome and backend usability depend on that backend. The
    /// manager does not guarantee rollback or safe retry in that case. The recovery
    /// limits on [`Branches`] also apply.
    /// A panic during file/metadata publication blocks further publication;
    /// reopen to recover.
    pub fn commit(&self, branch_id: BranchId) -> Result<CommitId> {
        self.with_slot(branch_id, BranchError::Conflict, |slot, origin| {
            let state = slot.as_mut().ok_or(BranchError::HandleInvalid)?;
            let sealed = state.seal(origin, &self.inner.hasher)?;
            let outcome = if let Some(writer) = &self.inner.immutable_writer {
                let mut writer = writer
                    .lock()
                    .map_err(|_| BranchError::Poisoned("immutable writer"))?;
                // Release the writer lock before propagating panics. File
                // publication remains blocked until reopen/recovery after a panic.
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    crate::commit::publish(
                        &mut writer,
                        &self.inner.store,
                        state.commit_id,
                        &sealed,
                        &state.immutable_data,
                        &self.inner.immutable_segments,
                    )
                }));
                drop(writer);
                match outcome {
                    Ok(result) => result,
                    Err(panic) => resume_unwind(panic),
                }
            } else {
                // Without immutable files, the store writer serializes commits.
                crate::commit::persist(self.inner.store.begin_write()?, state.commit_id, &sealed)
            };
            match outcome {
                Ok(commit_id) => {
                    *slot = None;
                    // Publication has succeeded. Registry housekeeping must not
                    // turn that durable success into an apparent commit failure.
                    // Queued callers already holding this entry see None.
                    let _ = self.remove(branch_id);
                    Ok(commit_id)
                }
                Err(BranchError::Conflict) => {
                    *slot = None;
                    self.remove(branch_id)?;
                    Err(BranchError::Conflict)
                }
                Err(error) => Err(error),
            }
        })?
    }

    /// Consume a live branch without reverse replay. A stale or already
    /// discarded ID returns `HandleInvalid`; stale state is still released.
    pub fn discard(&self, branch_id: BranchId) -> Result<()> {
        self.with_slot(branch_id, BranchError::HandleInvalid, |slot, _| {
            *slot = None;
        })?;
        self.remove(branch_id)
    }

    fn remove(&self, branch_id: BranchId) -> Result<()> {
        self.inner
            .branches
            .lock()
            .map_err(|_| BranchError::Poisoned("registry"))?
            .remove(&branch_id);
        Ok(())
    }

    /// Ordinary operations receive a live state, never its registry slot.
    fn with_branch<T>(
        &self,
        branch_id: BranchId,
        operation: impl FnOnce(&mut BranchState, &S::Read<'_>) -> T,
    ) -> Result<T> {
        self.with_slot(branch_id, BranchError::HandleInvalid, |slot, origin| {
            let state = slot.as_mut().ok_or(BranchError::HandleInvalid)?;
            Ok(operation(state, origin))
        })?
    }

    /// Keep slot removal, locking, head validation, and panic handling in one
    /// place. Only discard and commit need direct slot access; other operations use
    /// `with_branch` so they cannot remove or replace the state.
    fn with_slot<T>(
        &self,
        branch_id: BranchId,
        stale_error: BranchError,
        operation: impl FnOnce(&mut Option<BranchState>, &S::Read<'_>) -> T,
    ) -> Result<T> {
        let entry = self
            .inner
            .branches
            .lock()
            .map_err(|_| BranchError::Poisoned("registry"))?
            .get(&branch_id)
            .cloned()
            .ok_or(BranchError::HandleInvalid)?;
        // Never wait for a branch while holding the registry lock.
        #[cfg(not(test))]
        let mut slot = entry.lock().map_err(|_| BranchError::Poisoned("branch"))?;
        #[cfg(test)]
        let mut slot =
            crate::test_sync::lock(&entry).map_err(|_| BranchError::Poisoned("branch"))?;
        let commit_id = slot.as_ref().ok_or(BranchError::HandleInvalid)?.commit_id;
        let origin = self.inner.store.begin_read()?;
        if read_head(&origin)? != commit_id {
            *slot = None;
            self.remove(branch_id)?;
            return Err(stale_error);
        }
        // CellOverlay's write guard restores all partial mutations on unwind.
        // Unlock before resuming a callback panic so it cannot poison the lock.
        let result = catch_unwind(AssertUnwindSafe(|| operation(&mut slot, &origin)));
        drop(slot);
        match result {
            Ok(value) => Ok(value),
            Err(panic) => resume_unwind(panic),
        }
    }
}

fn allocate_branch_id(counter: &AtomicU64) -> Result<BranchId> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        })
        .map(BranchId::new)
        .map_err(|_| BranchError::BranchNumberExhausted)
}

#[cfg(test)]
#[path = "tests/immutable.rs"]
mod immutable_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_counter_never_wraps_or_reuses_numbers() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            allocate_branch_id(&counter).unwrap(),
            crate::BranchId::new(u64::MAX - 1)
        );
        for _ in 0..2 {
            assert!(matches!(
                allocate_branch_id(&counter),
                Err(BranchError::BranchNumberExhausted)
            ));
        }
    }
}
