//! Uncommitted-to-the-root metadata. Callers own publication and must abort their
//! transaction on mutation errors. These helpers never inspect the database head.
use crate::{ImmutableDataKey, ImmutableDataStorageError as Error, StorageResult};
use golemdb_storage::{ReadTransaction, Table, WriteTransaction};
use std::ops::Range;

pub const KEYS: Table = Table("ImmutableDataKeys");
pub const COMMIT_RANGES: Table = Table("ImmutableDataCommitRanges");

fn forward(segment: u32, key: ImmutableDataKey) -> Vec<u8> {
    let mut bytes = vec![0];
    bytes.extend(segment.to_be_bytes());
    bytes.extend(key.0);
    bytes
}
fn reverse(segment: u32, ordinal: u64) -> Vec<u8> {
    let mut bytes = vec![1];
    bytes.extend(segment.to_be_bytes());
    bytes.extend(ordinal.to_be_bytes());
    bytes
}
fn range_key(segment: u32, commit: u64) -> Vec<u8> {
    let mut bytes = segment.to_be_bytes().to_vec();
    bytes.extend(commit.to_be_bytes());
    bytes
}

/// Validate both directions of an existing key binding.
pub fn ordinal_for_key(
    tx: &impl ReadTransaction,
    segment: u32,
    key: ImmutableDataKey,
) -> StorageResult<Option<u64>> {
    let Some(bytes) = tx.get(KEYS, &forward(segment, key))? else {
        return Ok(None);
    };
    let ordinal = u64::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| Error::Corrupt("invalid key binding"))?,
    );
    if tx.get(KEYS, &reverse(segment, ordinal))?.as_deref() != Some(key.0.as_slice()) {
        return Err(Error::Corrupt("key binding has no matching reverse entry"));
    }
    Ok(Some(ordinal))
}
pub fn key_for_ordinal(
    tx: &impl ReadTransaction,
    segment: u32,
    ordinal: u64,
) -> StorageResult<Option<ImmutableDataKey>> {
    let Some(bytes) = tx.get(KEYS, &reverse(segment, ordinal))? else {
        return Ok(None);
    };
    let key = ImmutableDataKey(
        bytes
            .try_into()
            .map_err(|_| Error::Corrupt("invalid reverse binding"))?,
    );
    if ordinal_for_key(tx, segment, key)? != Some(ordinal) {
        return Err(Error::Corrupt("reverse binding has no matching key"));
    }
    Ok(Some(key))
}
pub fn bind_key(
    tx: &mut impl WriteTransaction,
    segment: u32,
    key: ImmutableDataKey,
    ordinal: u64,
) -> StorageResult<()> {
    if tx.get(KEYS, &forward(segment, key))?.is_some()
        || tx.get(KEYS, &reverse(segment, ordinal))?.is_some()
    {
        return Err(Error::AlreadyExists);
    }
    tx.insert(KEYS, &forward(segment, key), &ordinal.to_be_bytes())?;
    tx.insert(KEYS, &reverse(segment, ordinal), &key.0)?;
    Ok(())
}

/// Insert one nonempty range; empty batches should not call this helper. File
/// bounds are checked by ImmutableDataStorage::record_commit_range, not here.
pub fn record_commit_range(
    tx: &mut impl WriteTransaction,
    segment: u32,
    commit: u64,
    range: Range<u64>,
) -> StorageResult<()> {
    if range.start >= range.end {
        return Err(Error::Invalid("commit range must be nonempty"));
    }
    let key = range_key(segment, commit);
    if tx.get(COMMIT_RANGES, &key)?.is_some() {
        return Err(Error::AlreadyExists);
    }
    let mut value = range.start.to_be_bytes().to_vec();
    value.extend(range.end.to_be_bytes());
    tx.insert(COMMIT_RANGES, &key, &value)?;
    Ok(())
}
pub fn commit_range(
    tx: &impl ReadTransaction,
    segment: u32,
    commit: u64,
) -> StorageResult<Option<Range<u64>>> {
    let Some(bytes) = tx.get(COMMIT_RANGES, &range_key(segment, commit))? else {
        return Ok(None);
    };
    if bytes.len() != 16 {
        return Err(Error::Corrupt("invalid commit range encoding"));
    }
    let start = u64::from_be_bytes(bytes[..8].try_into().expect("checked length"));
    let end = u64::from_be_bytes(bytes[8..].try_into().expect("checked length"));
    if start >= end {
        return Err(Error::Corrupt("invalid commit range bounds"));
    }
    Ok(Some(start..end))
}
