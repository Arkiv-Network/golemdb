use crate::{
    ImmutableDataAddress, ImmutableDataReadOptions, ImmutableDataRow, ImmutableDataSchema,
    ImmutableDataStorageError as Error, StorageResult, backend::Reader, metadata,
    storage::Catalogue,
};
use golemdb_storage::ReadTransaction;
use std::sync::Arc;

/// Committed reads independent of the append writer. Supply one published KV
/// snapshot and its commit number for each call; this crate never reads head.
/// Values own their bytes and no file mappings escape a call. Clones keep the
/// directory owned until the last writer/reader is dropped. Retention is deferred.
#[derive(Clone)]
pub struct ImmutableDataReader {
    pub(crate) catalogue: Arc<Catalogue>,
    pub(crate) backend: Reader,
}
impl ImmutableDataReader {
    pub fn schema(&self, segment: &str) -> StorageResult<&ImmutableDataSchema> {
        Ok(&self.catalogue.segment(segment)?.schema)
    }

    /// Read a row visible in `tx` at `published_commit`. These must describe the
    /// same published snapshot, not an uncommitted writer or separately read head.
    pub fn get(
        &self,
        tx: &impl ReadTransaction,
        published_commit: u64,
        segment: &str,
        address: ImmutableDataAddress,
        options: &ImmutableDataReadOptions,
    ) -> StorageResult<ImmutableDataRow> {
        let segment = self.catalogue.segment(segment)?;
        let projection = segment.schema.projection(options)?;
        let latest_batch = metadata::last_range_at(tx, segment.id, published_commit)?;
        let end = latest_batch.as_ref().map_or(0, |(_, range)| range.end);
        let ordinal = match address {
            ImmutableDataAddress::Ordinal(ordinal) => {
                if ordinal >= end {
                    return Err(Error::NotFound);
                }
                ordinal
            }
            ImmutableDataAddress::Key(key) => {
                let ordinal =
                    metadata::ordinal_for_key(tx, segment.id, key)?.ok_or(Error::NotFound)?;
                if ordinal >= end {
                    return Err(Error::Corrupt("key exceeds published end"));
                }
                ordinal
            }
        };
        let shard_start_commit = metadata::shard_start_for_ordinal(
            tx,
            segment.id,
            ordinal,
            latest_batch.expect("nonempty published prefix"),
            self.catalogue.config.shard_span,
        )?;
        let mut rows = self
            .backend
            .read_range(
                segment,
                shard_start_commit,
                ordinal..ordinal + 1,
                projection.indices(),
            )
            .map_err(missing_row)?;
        let raw = rows
            .pop()
            .ok_or(Error::Corrupt("missing published immutable row"))?;
        Ok(projection.decode(&raw.iter().map(Vec::as_slice).collect::<Vec<_>>())?)
    }

    /// Read a commit's nonempty batch, or an empty vector when it appended no
    /// rows. `published_commit` must come from the same published snapshot `tx`.
    pub fn rows_of(
        &self,
        tx: &impl ReadTransaction,
        published_commit: u64,
        segment: &str,
        commit: u64,
        options: &ImmutableDataReadOptions,
    ) -> StorageResult<Vec<ImmutableDataRow>> {
        let segment = self.catalogue.segment(segment)?;
        let projection = segment.schema.projection(options)?;
        if commit > published_commit {
            return Err(Error::CommitUnavailable {
                requested: commit,
                published: published_commit,
            });
        }
        let Some(range) = metadata::commit_range(tx, segment.id, commit)? else {
            return Ok(vec![]);
        };
        self.backend
            .read_range(segment, commit, range, projection.indices())
            .map_err(missing_row)?
            .into_iter()
            .map(|row| Ok(projection.decode(&row.iter().map(Vec::as_slice).collect::<Vec<_>>())?))
            .collect()
    }
}
fn missing_row(error: Error) -> Error {
    match error {
        Error::NotFound | Error::Invalid(_) => Error::Corrupt("missing published immutable rows"),
        other => other,
    }
}
