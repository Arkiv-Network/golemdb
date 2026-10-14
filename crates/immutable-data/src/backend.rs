use crate::{
    ImmutableDataStorageError as Error, StorageResult, files::NippyJarStorage,
    memory::MemoryStorage, storage::Segment,
};
use std::ops::Range;
pub(crate) type PayloadRow = Vec<Vec<u8>>;

/// Private physical dispatch. Neither backend owns a database transaction.
pub(crate) enum Backend {
    NippyJar(NippyJarStorage),
    Memory(MemoryStorage),
}
impl Backend {
    pub(crate) fn append(&mut self, s: &Segment, commit: u64, row: &[&[u8]]) -> StorageResult<u64> {
        match self {
            Self::NippyJar(f) => f.append(s, commit, row),
            Self::Memory(m) => m.append(s, commit, row),
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
            Self::Memory(m) => m.get(s, ordinal, columns),
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
            Self::Memory(m) => m.read_range(s, commit, range, columns),
        }
    }
    pub(crate) fn end_ordinal(&mut self, s: &Segment) -> StorageResult<u64> {
        match self {
            Self::NippyJar(f) => f.end_ordinal(s),
            Self::Memory(m) => Ok(m.end_ordinal(s)),
        }
    }
    pub(crate) fn truncate(&mut self, s: &Segment, from: u64) -> StorageResult<()> {
        match self {
            Self::NippyJar(f) => f.truncate(s, from),
            Self::Memory(m) => m.truncate(s, from),
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
