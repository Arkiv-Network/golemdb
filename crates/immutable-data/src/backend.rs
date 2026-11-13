use crate::{
    ImmutableDataStorageError as Error, StorageResult,
    files::{FileSet, NippyJarStorage},
    memory::MemoryStorage,
    storage::Segment,
};
use std::{
    ops::Range,
    sync::{Arc, RwLock},
};
pub(crate) type PayloadRow = Vec<Vec<u8>>;

/// Private physical dispatch. Neither backend owns a database transaction.
pub(crate) enum Backend {
    NippyJar(NippyJarStorage),
    Memory(Arc<RwLock<MemoryStorage>>),
}
impl Backend {
    pub(crate) fn reader(&self) -> Reader {
        match self {
            Self::NippyJar(files) => Reader::NippyJar(files.reader()),
            Self::Memory(memory) => Reader::Memory(memory.clone()),
        }
    }

    pub(crate) fn recover(
        &mut self,
        s: &Segment,
        ranges: &std::collections::BTreeMap<u64, Range<u64>>,
    ) -> StorageResult<()> {
        match self {
            Self::NippyJar(f) => f.recover(s, ranges),
            Self::Memory(m) => m
                .write()
                .map_err(|_| Error::Invalid("memory lock poisoned"))?
                .recover(s, ranges),
        }
    }
    pub(crate) fn append(&mut self, s: &Segment, commit: u64, row: &[&[u8]]) -> StorageResult<u64> {
        match self {
            Self::NippyJar(f) => f.append(s, commit, row),
            Self::Memory(m) => m
                .write()
                .map_err(|_| Error::Invalid("memory lock poisoned"))?
                .append(s, commit, row),
        }
    }
    pub(crate) fn get(
        &mut self,
        s: &Segment,
        ordinal: u64,
        columns: &[usize],
    ) -> StorageResult<PayloadRow> {
        match self {
            Self::NippyJar(f) => f.get(s, ordinal, columns),
            Self::Memory(m) => m
                .read()
                .map_err(|_| Error::Invalid("memory lock poisoned"))?
                .get(s, ordinal, columns),
        }
    }
    pub(crate) fn read_range(
        &mut self,
        s: &Segment,
        commit: u64,
        range: Range<u64>,
        columns: &[usize],
    ) -> StorageResult<Vec<PayloadRow>> {
        match self {
            Self::NippyJar(f) => f.read_range(s, commit, range, columns),
            Self::Memory(m) => m
                .read()
                .map_err(|_| Error::Invalid("memory lock poisoned"))?
                .read_range(s, commit, range, columns),
        }
    }
    pub(crate) fn end_ordinal(&mut self, s: &Segment) -> StorageResult<u64> {
        match self {
            Self::NippyJar(f) => f.end_ordinal(s),
            Self::Memory(m) => Ok(m
                .read()
                .map_err(|_| Error::Invalid("memory lock poisoned"))?
                .end_ordinal(s)),
        }
    }
    pub(crate) fn truncate(&mut self, s: &Segment, from: u64) -> StorageResult<()> {
        match self {
            Self::NippyJar(f) => f.truncate(s, from),
            Self::Memory(m) => m
                .write()
                .map_err(|_| Error::Invalid("memory lock poisoned"))?
                .truncate(s, from),
        }
    }
    pub(crate) fn sync(&mut self) -> StorageResult<()> {
        match self {
            Self::NippyJar(f) => f.sync(),
            Self::Memory(_) => Ok(()),
        }
    }
}

/// Commit is only a routing coordinate. Check the exclusive end for overflow.
pub(crate) fn span_start(commit: u64, span: u64) -> StorageResult<u64> {
    let first = commit / span * span;
    first.checked_add(span).ok_or(Error::Overflow)?;
    Ok(first)
}

/// Read-only dispatch has no access to active NippyJar writers.
#[derive(Clone)]
pub(crate) enum Reader {
    NippyJar(Arc<FileSet>),
    Memory(Arc<RwLock<MemoryStorage>>),
}
impl Reader {
    pub(crate) fn read_range(
        &self,
        s: &Segment,
        commit: u64,
        range: Range<u64>,
        columns: &[usize],
    ) -> StorageResult<Vec<PayloadRow>> {
        match self {
            Self::NippyJar(f) => f.read_range(s, commit, range, columns),
            Self::Memory(m) => m
                .read()
                .map_err(|_| Error::Invalid("memory lock poisoned"))?
                .read_range(s, commit, range, columns),
        }
    }
}
