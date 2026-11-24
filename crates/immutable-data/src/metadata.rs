//! Uncommitted-to-the-root metadata. Callers own publication and must abort their
//! transaction on mutation errors. These helpers never inspect the database head.
use crate::{ImmutableDataKey, ImmutableDataStorageError as Error, StorageResult};
use golemdb_storage::{ReadCursor, ReadTransaction, Table, WriteTransaction};
use std::ops::Range;

pub const KEYS: Table = Table("ImmutableDataKeys");
pub const COMMIT_RANGES: Table = Table("ImmutableDataCommitRanges");

const SEGMENT_ID_LEN: usize = size_of::<u32>();
/// Commit range keys encode segment:u32_be || commit:u64_be (4 + 8 bytes).
const COMMIT_RANGE_KEY_LEN: usize = SEGMENT_ID_LEN + size_of::<u64>();

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
    decode_range(&bytes).map(Some)
}

/// Exclusive end from the latest recorded range at or before `commit`, or zero
/// if this segment has no such range. The caller supplies a published snapshot
/// and verifies commit visibility. No files or database head are consulted.
/// Retention must preserve this allocation boundary; pruning is not implemented.
pub fn end_ordinal_at(tx: &impl ReadTransaction, segment: u32, commit: u64) -> StorageResult<u64> {
    Ok(last_range_at(tx, segment, commit)?.map_or(0, |(_, range)| range.end))
}

/// Latest nonempty batch at/before the supplied publication snapshot. Its
/// commit identifies the newest file span a committed reader may inspect.
pub(crate) fn last_range_at(
    tx: &impl ReadTransaction,
    segment: u32,
    commit: u64,
) -> StorageResult<Option<(u64, Range<u64>)>> {
    let mut cursor = tx.cursor(COMMIT_RANGES, &range_key(segment, commit))?;
    let Some((key, value)) = cursor.prev()? else {
        return Ok(None);
    };
    if !key.starts_with(&segment.to_be_bytes()) {
        return Ok(None);
    }
    if key.len() != COMMIT_RANGE_KEY_LEN {
        return Err(Error::Corrupt("invalid commit range key"));
    }
    let commit = u64::from_be_bytes(key[SEGMENT_ID_LEN..].try_into().expect("checked length"));
    Ok(Some((commit, decode_range(&value)?)))
}

/// Return the starting commit of the shard containing a visible ordinal.
/// `candidate_last_batch` is (commit ID, appended ordinal range), initially the
/// segment's last published batch from the caller's snapshot. Seek once per
/// populated shard, skipping individual commits and entirely empty spans.
pub(crate) fn shard_start_for_ordinal(
    tx: &impl ReadTransaction,
    segment: u32,
    ordinal: u64,
    mut candidate_last_batch: (u64, Range<u64>),
    shard_span: u64,
) -> StorageResult<u64> {
    let published_end = candidate_last_batch.1.end;
    if ordinal >= published_end {
        return Err(Error::NotFound);
    }
    loop {
        let (candidate_commit, candidate_range) = &candidate_last_batch;
        let shard_start_commit = crate::backend::span_start(*candidate_commit, shard_span)?;
        let previous_last_batch = match shard_start_commit.checked_sub(1) {
            Some(before) => last_range_at(tx, segment, before)?,
            None => None,
        };
        // Ordinals are contiguous across shards. Retention must preserve this
        // boundary (or its equivalent) before older metadata can be pruned.
        let first_ordinal = previous_last_batch
            .as_ref()
            .map_or(0, |(_, range)| range.end);
        // Use the preceding shard's end rather than this batch's start: the
        // batch may start partway through the shard. This also keeps an intact
        // older prefix readable if a later batch's start was damaged.
        if first_ordinal > candidate_range.end {
            return Err(Error::Corrupt("decreasing published shard ends"));
        }
        if ordinal >= first_ordinal {
            return Ok(shard_start_commit);
        }
        candidate_last_batch =
            previous_last_batch.expect("ordinal precedes a nonzero shard boundary");
    }
}

fn decode_range(bytes: &[u8]) -> StorageResult<Range<u64>> {
    if bytes.len() != 16 {
        return Err(Error::Corrupt("invalid commit range encoding"));
    }
    let start = u64::from_be_bytes(bytes[..8].try_into().expect("checked length"));
    let end = u64::from_be_bytes(bytes[8..].try_into().expect("checked length"));
    if start >= end {
        return Err(Error::Corrupt("invalid commit range bounds"));
    }
    Ok(start..end)
}

/// Temporary recovery inventory, grouped by file span. Reject gaps and future
/// metadata rather than guessing a truncation boundary. Retention is not enabled.
pub(crate) fn recovery_ranges(
    tx: &impl ReadTransaction,
    segment: u32,
    commit: u64,
    span: u64,
) -> StorageResult<std::collections::BTreeMap<u64, Range<u64>>> {
    let prefix = segment.to_be_bytes();
    let mut cursor = tx.cursor(COMMIT_RANGES, &prefix)?;
    let mut ranges = std::collections::BTreeMap::<u64, Range<u64>>::new();
    let mut end = 0;
    while let Some((key, value)) = cursor.next()? {
        if !key.starts_with(&prefix) {
            break;
        }
        if key.len() != COMMIT_RANGE_KEY_LEN {
            return Err(Error::Corrupt("invalid commit range key"));
        }
        let id = u64::from_be_bytes(
            key[SEGMENT_ID_LEN..]
                .try_into()
                .expect("checked key length"),
        );
        if id > commit {
            return Err(Error::Corrupt("commit range exceeds published head"));
        }
        let range = decode_range(&value)?;
        if range.start != end {
            return Err(Error::Corrupt("noncontiguous published ranges"));
        }
        end = range.end;
        let start = crate::backend::span_start(id, span)?;
        ranges
            .entry(start)
            .and_modify(|r| r.end = range.end)
            .or_insert(range);
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use golemdb_storage::{MemoryStore, Store};
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn ordinal_routing_matches_the_appending_commit(
            span in 1u64..100,
            batches in prop::collection::vec((0u64..200, 1u64..5), 1..30),
        ) {
            let store = MemoryStore::new();
            let mut tx = store.begin_write().unwrap();
            // A predecessor seek must not borrow another segment's boundary.
            record_commit_range(&mut tx, 0, u64::MAX, 0..999).unwrap();
            record_commit_range(&mut tx, 2, 0, 0..999).unwrap();
            let mut commit = 0;
            let mut end = 0;
            let mut expected = Vec::new();
            for (gap, count) in batches {
                commit += gap;
                record_commit_range(&mut tx, 1, commit, end..end + count).unwrap();
                expected.extend(std::iter::repeat_n(commit / span * span, count as usize));
                end += count;
                // Exercise every publication prefix, including heads beyond the
                // last append, with empty commits and entirely skipped shards.
                let latest = last_range_at(&tx, 1, commit + 200).unwrap().unwrap();
                for (ordinal, &shard) in expected.iter().enumerate() {
                    prop_assert_eq!(
                        shard_start_for_ordinal(&tx, 1, ordinal as u64, latest.clone(), span).unwrap(),
                        shard,
                    );
                }
                commit += 1;
            }
        }
    }

    #[test]
    fn shard_zero_and_first_append_in_a_later_shard() {
        for first_commit in [0, 9] {
            let store = MemoryStore::new();
            let mut tx = store.begin_write().unwrap();
            record_commit_range(&mut tx, 0, 100, 0..100).unwrap();
            record_commit_range(&mut tx, 1, first_commit, 0..2).unwrap();
            record_commit_range(&mut tx, 1, first_commit + 1, 2..3).unwrap();
            let latest = last_range_at(&tx, 1, 100).unwrap().unwrap();
            assert_eq!(
                shard_start_for_ordinal(&tx, 1, 0, latest, 4).unwrap(),
                first_commit / 4 * 4,
            );
        }
    }

    #[test]
    fn routing_rejects_decreasing_shard_ends() {
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        record_commit_range(&mut tx, 1, 10, 0..20).unwrap();
        // Each batch is individually valid, but the newer shard's end goes backward.
        record_commit_range(&mut tx, 1, 110, 10..15).unwrap();
        tx.commit().unwrap();
        let tx = store.begin_read().unwrap();
        let latest_batch = last_range_at(&tx, 1, 110).unwrap().unwrap();
        assert!(matches!(
            shard_start_for_ordinal(&tx, 1, 12, latest_batch, 100),
            Err(Error::Corrupt("decreasing published shard ends")),
        ));
    }

    #[test]
    fn routing_seeks_by_shard_not_by_commit_or_empty_span() {
        struct CountSeeks<T> {
            inner: T,
            seeks: std::cell::Cell<usize>,
        }
        impl<T: ReadTransaction> ReadTransaction for CountSeeks<T> {
            type Cursor<'tx>
                = T::Cursor<'tx>
            where
                Self: 'tx;

            fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
                self.inner.get(table, key)
            }

            fn cursor(
                &self,
                table: Table,
                key: &[u8],
            ) -> golemdb_storage::Result<Self::Cursor<'_>> {
                self.seeks.set(self.seeks.get() + 1);
                self.inner.cursor(table, key)
            }

            fn with_value<Output>(
                &self,
                table: golemdb_storage::Table,
                key: &[u8],
                inspect: impl FnOnce(Option<&[u8]>) -> Output,
            ) -> golemdb_storage::Result<Output> {
                self.inner.with_value(table, key, inspect)
            }
        }
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        record_commit_range(&mut tx, 1, 10, 0..1).unwrap();
        for i in 0..100 {
            record_commit_range(&mut tx, 1, 1000 + i, i + 1..i + 2).unwrap();
        }
        record_commit_range(&mut tx, 1, 1_000_050, 101..102).unwrap();
        tx.commit().unwrap();
        let tx = CountSeeks {
            inner: store.begin_read().unwrap(),
            seeks: std::cell::Cell::new(0),
        };
        let latest = last_range_at(&tx, 1, 2_000_000).unwrap().unwrap();
        for (ordinal, shard, seeks) in [(101, 1_000_000, 1), (50, 1000, 2), (0, 0, 2)] {
            tx.seeks.set(0);
            assert_eq!(
                shard_start_for_ordinal(&tx, 1, ordinal, latest.clone(), 100).unwrap(),
                shard
            );
            assert_eq!(tx.seeks.get(), seeks);
        }
    }
}
