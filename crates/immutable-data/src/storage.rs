use crate::{
    ImmutableDataCompression, ImmutableDataConfig, ImmutableDataKey, ImmutableDataReadOptions,
    ImmutableDataRow, ImmutableDataSchema, ImmutableDataStorageError as Error, StorageResult,
    backend::Backend, files::NippyJarStorage, memory::MemoryStorage, metadata,
};
use golemdb_storage::{ReadTransaction, WriteTransaction};
use std::{
    collections::BTreeMap,
    ops::Range,
    path::Path,
    sync::{Mutex, MutexGuard},
};

/// Validated genesis declaration shared with the private physical backends.
pub(crate) struct Segment {
    pub id: u32,
    pub schema: ImmutableDataSchema,
    pub compression: ImmutableDataCompression,
}

/// Physical row storage plus caller-owned metadata conveniences.
///
/// This handle has no branch or publication state. The caller must serialize the
/// entire append/sync/KV-publication/rollback sequence and exclude readers during
/// it. The internal mutex only supports flushing a buffered writer during reads;
/// it does not provide database snapshot isolation. Call `sync` before publishing
/// metadata or closing storage. Drop does not guarantee durable appends. After
/// reopening, file access discards unfinished tails beyond the configured row
/// count; missing configured data remains an error.
pub struct ImmutableDataStorage {
    segments: Vec<Segment>,
    names: BTreeMap<String, usize>,
    backend: Mutex<Backend>,
}

impl ImmutableDataStorage {
    pub fn open(
        config: ImmutableDataConfig,
        genesis_id: [u8; 32],
        directory: impl AsRef<Path>,
    ) -> StorageResult<Self> {
        let span = config.shard_span;
        let mut storage = Self::in_memory(config, genesis_id)?;
        storage.backend = Mutex::new(Backend::NippyJar(NippyJarStorage::open(
            directory, genesis_id, span,
        )?));
        Ok(storage)
    }

    pub fn in_memory(config: ImmutableDataConfig, _genesis_id: [u8; 32]) -> StorageResult<Self> {
        let segments = config
            .segments_by_id()?
            .into_iter()
            .map(|(id, s)| {
                Ok(Segment {
                    id,
                    schema: s.schema()?,
                    compression: s.compression,
                })
            })
            .collect::<StorageResult<Vec<_>>>()?;
        let names = segments
            .iter()
            .enumerate()
            .map(|(i, s)| (s.schema.name().to_owned(), i))
            .collect();
        Ok(Self {
            segments,
            names,
            backend: Mutex::new(Backend::Memory(MemoryStorage::new(config.shard_span))),
        })
    }

    pub fn schema(&self, segment: &str) -> StorageResult<&ImmutableDataSchema> {
        Ok(&self.segment(segment)?.schema)
    }

    /// Append one complete typed row. `commit` selects the file's commit span;
    /// this does not publish a database commit or bind a key. Multiple appends
    /// reuse the active writer and are made durable together by `sync`.
    pub fn append(
        &mut self,
        segment: &str,
        commit: u64,
        row: &ImmutableDataRow,
    ) -> StorageResult<u64> {
        let segment = self.segment(segment)?;
        segment.schema.validate_row(row)?;
        let payloads: Vec<_> = row.payloads().collect();
        payloads
            .iter()
            .try_fold(0usize, |n, v| n.checked_add(v.len()).ok_or(Error::Overflow))?;
        self.backend()?.append(segment, commit, &payloads)
    }

    /// Read a physical row, regardless of database publication. A read may flush
    /// the active writer so NippyJar can access its offsets and configuration.
    pub fn get(
        &self,
        segment: &str,
        ordinal: u64,
        options: &ImmutableDataReadOptions,
    ) -> StorageResult<ImmutableDataRow> {
        let segment = self.segment(segment)?;
        let projection = segment.schema.projection(options)?;
        let raw = self
            .backend()?
            .get(segment, ordinal, projection.indices())?;
        Ok(projection.decode(&raw.iter().map(Vec::as_slice).collect::<Vec<_>>())?)
    }

    /// Exclusive physical end, including successful buffered appends; zero for
    /// an empty segment. The coordinator remembers this before appending a batch.
    pub fn end_ordinal(&self, segment: &str) -> StorageResult<u64> {
        self.backend()?.end_ordinal(self.segment(segment)?)
    }

    /// Remove the physical tail [from_ordinal, end), including later files.
    /// Does not modify key/range metadata. The caller owns rollback and must not
    /// truncate data still referenced by published metadata.
    pub fn truncate(&mut self, segment: &str, from_ordinal: u64) -> StorageResult<()> {
        self.backend()?
            .truncate(self.segment(segment)?, from_ordinal)
    }

    /// Durably flush all pending file changes before the caller publishes KV.
    pub fn sync(&mut self) -> StorageResult<()> {
        self.backend()?.sync()
    }

    /// Stage a segment-scoped key binding in the caller's transaction. The row
    /// must already exist physically. Abort the transaction on any mutation error.
    pub fn bind_key(
        &self,
        tx: &mut impl WriteTransaction,
        segment: &str,
        key: ImmutableDataKey,
        ordinal: u64,
    ) -> StorageResult<()> {
        let segment = self.segment(segment)?;
        self.backend()?.get(segment, ordinal, &[])?;
        metadata::bind_key(tx, segment.id, key, ordinal)
    }

    pub fn get_by_key(
        &self,
        tx: &impl ReadTransaction,
        segment: &str,
        key: ImmutableDataKey,
        options: &ImmutableDataReadOptions,
    ) -> StorageResult<ImmutableDataRow> {
        let id = self.segment(segment)?.id;
        let ordinal = metadata::ordinal_for_key(tx, id, key)?.ok_or(Error::NotFound)?;
        self.get(segment, ordinal, options)
            .map_err(missing_metadata_row)
    }

    /// Record one nonempty batch's ordinal range. The caller establishes the
    /// commit's identity and publication; storage only validates physical bounds.
    pub fn record_commit_range(
        &self,
        tx: &mut impl WriteTransaction,
        segment: &str,
        commit: u64,
        range: Range<u64>,
    ) -> StorageResult<()> {
        if range.start >= range.end {
            return Err(Error::Invalid("commit range must be nonempty"));
        }
        let segment = self.segment(segment)?;
        self.backend()?
            .read_range(segment, commit, range.clone(), &[])?;
        metadata::record_commit_range(tx, segment.id, commit, range)
    }

    /// Read a caller-published commit range with projection. No entry returns an
    /// empty vector; the caller checks whether the commit exists and is visible.
    pub fn rows_of(
        &self,
        tx: &impl ReadTransaction,
        segment: &str,
        commit: u64,
        options: &ImmutableDataReadOptions,
    ) -> StorageResult<Vec<ImmutableDataRow>> {
        let segment = self.segment(segment)?;
        let projection = segment.schema.projection(options)?;
        let Some(range) = metadata::commit_range(tx, segment.id, commit)? else {
            return Ok(vec![]);
        };
        self.backend()?
            .read_range(segment, commit, range, projection.indices())
            .map_err(|err| match err {
                Error::NotFound | Error::Invalid(_) => {
                    Error::Corrupt("commit range refers to missing physical rows")
                }
                other => other,
            })?
            .into_iter()
            .map(|row| Ok(projection.decode(&row.iter().map(Vec::as_slice).collect::<Vec<_>>())?))
            .collect()
    }

    fn segment(&self, name: &str) -> StorageResult<&Segment> {
        self.names
            .get(name)
            .map(|&i| &self.segments[i])
            .ok_or(Error::NotFound)
    }
    fn backend(&self) -> StorageResult<MutexGuard<'_, Backend>> {
        self.backend
            .lock()
            .map_err(|_| Error::Invalid("physical storage lock poisoned"))
    }
}

fn missing_metadata_row(err: Error) -> Error {
    match err {
        Error::NotFound => Error::Corrupt("key refers to a missing physical row"),
        other => other,
    }
}
