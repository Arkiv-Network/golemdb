use crate::{
    ImmutableDataCompression, ImmutableDataStorageError as Error, StorageResult,
    backend::{PayloadRow, span_start},
    storage::Segment,
};
use reth_nippy_jar::{
    NippyJar, NippyJarCursor, NippyJarWriter,
    compression::{Compression, Compressors},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, RwLock, RwLockWriteGuard},
};

/// Stored in NippyJar's .conf; physical row count and compression belong to jar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileHeader {
    pub format: u32,
    pub genesis_id: [u8; 32],
    pub segment: u32,
    pub first_commit: u64,
    pub first_ordinal: u64,
}

struct ActiveFile {
    path: PathBuf,
    writer: NippyJarWriter<FileHeader>,
    // A failed column append may leave a partial row. Never let another append
    // or physical read silently consume it. Published-prefix readers remain
    // independent; the publisher must recover before reusing a failed writer.
    failed: bool,
}
impl ActiveFile {
    fn new(jar: NippyJar<FileHeader>) -> StorageResult<Self> {
        let path = jar.data_path().to_owned();
        Ok(Self {
            path,
            writer: NippyJarWriter::new(jar)?,
            failed: false,
        })
    }
    fn end(&self) -> StorageResult<u64> {
        self.writer
            .user_header()
            .first_ordinal
            .checked_add(self.writer.rows() as u64)
            .ok_or(Error::Overflow)
    }
    fn check(&self) -> StorageResult<()> {
        if self.failed {
            Err(Error::Invalid("file writer failed; recovery required"))
        } else {
            Ok(())
        }
    }
    fn sync(&mut self) -> StorageResult<()> {
        self.check()?;
        if self.writer.is_dirty() {
            self.failed = true;
            self.writer.commit()?;
            File::open(self.path.with_extension("conf"))?.sync_all()?;
            self.failed = false;
        }
        Ok(())
    }
}

/// Shared directory identity, OS ownership lock and cleanup guard. Historical
/// configurations and mappings live for one read call, without a cache.
pub(crate) struct FileSet {
    directory: PathBuf,
    genesis_id: [u8; 32],
    span: u64,
    // Readers hold shared access for the lifetime of their local mmaps. Only
    // destructive tail cleanup takes exclusive access; ordinary append/sync do not.
    cleanup: RwLock<()>,
    _lock: File,
}
impl FileSet {
    pub(crate) fn open(
        directory: impl AsRef<Path>,
        genesis_id: [u8; 32],
        span: u64,
    ) -> StorageResult<Self> {
        let directory = std::path::absolute(directory)?;
        let missing: Vec<_> = directory.ancestors().take_while(|p| !p.exists()).collect();
        fs::create_dir_all(&directory)?;
        for path in missing.into_iter().rev() {
            File::open(path)?.sync_all()?;
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(".lock"))?;
        lock.try_lock().map_err(|_| {
            Error::Invalid("immutable directory already owned or locking unsupported")
        })?;
        Ok(Self {
            directory,
            genesis_id,
            span,
            cleanup: RwLock::new(()),
            _lock: lock,
        })
    }

    fn lock_cleanup(&self) -> StorageResult<RwLockWriteGuard<'_, ()>> {
        #[cfg(not(test))]
        let guard = self.cleanup.write();
        #[cfg(test)]
        let guard = test_sync::write(&self.cleanup);
        guard.map_err(|_| Error::Invalid("file cleanup lock poisoned"))
    }

    fn path(&self, segment: &Segment, start: u64) -> StorageResult<PathBuf> {
        let end = start.checked_add(self.span).ok_or(Error::Overflow)?;
        Ok(self.directory.join(format!(
            "segment-{}-{start}-{end}.jar",
            segment.schema.name()
        )))
    }

    /// Discover filenames only, sorting parsed u64 starts newest first. Include
    /// data, offsets, configuration and temporary configuration files so an
    /// incomplete file creation cannot make an existing tail disappear.
    fn starts(&self, segment: &Segment) -> StorageResult<Vec<u64>> {
        #[cfg(test)]
        test_sync::at(test_sync::Point::DirectoryScan)?;
        let prefix = format!("segment-{}-", segment.schema.name());
        let mut starts = BTreeSet::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Some((numbers, ext)) = rest.rsplit_once('.') else {
                continue;
            };
            if !["jar", "conf", "off", "tmp"].contains(&ext) {
                continue;
            }
            let Some((start, end)) = numbers.split_once('-') else {
                continue;
            };
            let (Ok(start), Ok(end)) = (start.parse::<u64>(), end.parse::<u64>()) else {
                continue;
            };
            if start.is_multiple_of(self.span)
                && start.checked_add(self.span) == Some(end)
                && numbers == format!("{start}-{end}")
            {
                if !entry.file_type()?.is_file() {
                    return Err(Error::Corrupt("shard path is not a regular file"));
                }
                starts.insert(start);
            }
        }
        Ok(starts.into_iter().rev().collect())
    }

    /// Header and native configuration only; no user payloads/offsets are read.
    fn configuration(&self, segment: &Segment, start: u64) -> StorageResult<NippyJar<FileHeader>> {
        let path = self.path(segment, start)?;
        if !path.with_extension("conf").is_file() {
            return Err(Error::Corrupt("missing shard configuration"));
        }
        let jar = NippyJar::<FileHeader>::load(&path)?;
        let h = jar.user_header();
        if h.format != 1
            || h.genesis_id != self.genesis_id
            || h.segment != segment.id
            || h.first_commit != start
        {
            return Err(Error::Corrupt(
                "file identity disagrees with genesis or filename",
            ));
        }
        if jar.columns() != segment.schema.columns().len() || jar.columns() > usize::BITS as usize {
            return Err(Error::Corrupt(
                "file columns disagree with schema or platform",
            ));
        }
        let compression = match jar.compressor() {
            None => ImmutableDataCompression::None,
            Some(Compressors::Lz4(_)) => ImmutableDataCompression::Lz4,
            Some(Compressors::Zstd(zstd)) if !zstd.use_dict => ImmutableDataCompression::Zstd,
            _ => return Err(Error::Corrupt("unsupported file compression")),
        };
        if compression != segment.compression {
            return Err(Error::Corrupt("file compression disagrees with schema"));
        }
        end(&jar)?;
        Ok(jar)
    }

    /// Drop can flush buffered payloads without publishing their offsets/config.
    /// Let NippyJar discard only that surplus, after checking the configured prefix.
    /// Recovery is needed before reads too: the last column of the last row uses EOF.
    fn recover_file(&self, jar: NippyJar<FileHeader>) -> StorageResult<NippyJar<FileHeader>> {
        match inspect_file(&jar)? {
            FileState::Consistent => Ok(jar),
            FileState::UnfinishedTail => {
                // Trimming files must wait for committed readers' mappings to drop.
                // Clean files and ordinary appends do not acquire this lock.
                let _cleanup = self.lock_cleanup()?;
                let configured_rows = jar.rows();
                // NippyJar heals and commits the tail, propagating I/O failures.
                // Only the configured row count needs an additional policy check.
                let jar = NippyJarWriter::new(jar)?.into_jar();
                if jar.rows() != configured_rows {
                    return Err(Error::Corrupt(
                        "shard recovery changed configured row count",
                    ));
                }
                File::open(jar.config_path())?.sync_all()?;
                Ok(jar)
            }
        }
    }

    fn read_rows(
        &self,
        jar: NippyJar<FileHeader>,
        range: Range<u64>,
        columns: &[usize],
        committed_prefix: bool,
    ) -> StorageResult<Vec<PayloadRow>> {
        check_range(&jar, &range)?;
        let jar = if committed_prefix {
            #[cfg(test)]
            test_sync::at(test_sync::Point::PrefixRead)?;
            validate_prefix(&jar, range.end)?;
            jar
        } else {
            self.recover_file(jar)?
        };
        if columns.is_empty() {
            return Ok(vec![]);
        }
        if columns
            .iter()
            .any(|&i| i >= jar.columns() || i >= usize::BITS as usize)
        {
            return Err(Error::Invalid("invalid projection"));
        }
        let base = jar.user_header().first_ordinal;
        let local = usize::try_from(range.start - base).map_err(|_| Error::Overflow)?
            ..usize::try_from(range.end - base).map_err(|_| Error::Overflow)?;
        if local.is_empty() {
            return Ok(vec![]);
        }
        if committed_prefix {
            return read_prefix_rows(&jar, local, columns);
        }
        let mut sorted = columns.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mask = sorted.iter().fold(0usize, |mask, &i| mask | (1usize << i));
        let positions: Vec<_> = columns
            .iter()
            .map(|i| sorted.binary_search(i).expect("selected column"))
            .collect();
        // Physical reads own the writer for this call, and recovery removed
        // unfinished tails, making the native cursor's EOF boundary valid. Basic
        // checks do not audit interior offsets; malformed ones may panic upstream.
        let mut cursor = NippyJarCursor::new(&jar)?;
        let mut rows = Vec::with_capacity(local.len());
        for ordinal in local {
            let values = cursor
                .row_by_number_with_cols(ordinal, mask)?
                .ok_or(Error::Corrupt("missing physical row"))?;
            rows.push(positions.iter().map(|&i| values[i].to_vec()).collect());
        }
        Ok(rows)
    }

    pub(crate) fn read_range(
        &self,
        segment: &Segment,
        commit: u64,
        range: Range<u64>,
        columns: &[usize],
    ) -> StorageResult<Vec<PayloadRow>> {
        let _read = self
            .cleanup
            .read()
            .map_err(|_| Error::Invalid("file cleanup lock poisoned"))?;
        // The caller resolves the shard from its published metadata snapshot.
        // Directory iteration can omit an atomically replaced .conf.
        let jar = self.configuration(segment, span_start(commit, self.span)?)?;
        self.read_rows(jar, range, columns, true)
    }
}

/// The single mutable writer owns buffers; cloneable FileSet handles own only
/// directory identity and cleanup coordination. Buffers drop before ownership.
pub(crate) struct NippyJarStorage {
    active: BTreeMap<u32, ActiveFile>,
    files: Arc<FileSet>,
}
impl NippyJarStorage {
    pub(crate) fn open(
        directory: impl AsRef<Path>,
        genesis_id: [u8; 32],
        span: u64,
    ) -> StorageResult<Self> {
        Ok(Self {
            active: BTreeMap::new(),
            files: Arc::new(FileSet::open(directory, genesis_id, span)?),
        })
    }
    pub(crate) fn reader(&self) -> Arc<FileSet> {
        self.files.clone()
    }

    pub(crate) fn end_ordinal(&self, segment: &Segment) -> StorageResult<u64> {
        if let Some(active) = self.active.get(&segment.id) {
            active.check()?;
            return active.end();
        }
        match self.files.starts(segment)?.first() {
            Some(&start) => end(&self.files.configuration(segment, start)?),
            None => Ok(0),
        }
    }

    pub(crate) fn append(
        &mut self,
        segment: &Segment,
        commit: u64,
        row: &[&[u8]],
    ) -> StorageResult<u64> {
        let start = span_start(commit, self.files.span)?;
        if row.len() != segment.schema.columns().len() || row.len() > usize::BITS as usize {
            return Err(Error::Invalid("invalid physical row arity"));
        }
        // On the steady append path no directory scan, configuration read or
        // writer reopen is necessary. A commit number only selects a file span.
        let (previous, ordinal, existing) = if let Some(active) = self.active.get(&segment.id) {
            active.check()?;
            (
                Some(active.writer.user_header().first_commit),
                active.end()?,
                None,
            )
        } else if let Some(&last) = self.files.starts(segment)?.first() {
            let jar = self
                .files
                .recover_file(self.files.configuration(segment, last)?)?;
            (Some(last), end(&jar)?, Some(jar))
        } else {
            (None, 0, None)
        };
        if previous.is_some_and(|p| start < p) {
            return Err(Error::Invalid("cannot append to an older commit span"));
        }
        ordinal.checked_add(1).ok_or(Error::Overflow)?;
        if previous != Some(start) {
            if let Some(active) = self.active.get_mut(&segment.id) {
                active.sync()?;
            }
            self.active.remove(&segment.id);
            let header = FileHeader {
                format: 1,
                genesis_id: self.files.genesis_id,
                segment: segment.id,
                first_commit: start,
                first_ordinal: ordinal,
            };
            let jar = NippyJar::new(row.len(), &self.files.path(segment, start)?, header);
            let jar = match segment.compression {
                ImmutableDataCompression::None => jar,
                ImmutableDataCompression::Lz4 => jar.with_lz4(),
                ImmutableDataCompression::Zstd => jar.with_zstd(false, 0),
            };
            self.active.insert(segment.id, ActiveFile::new(jar)?);
        } else if let Some(jar) = existing {
            self.active.insert(segment.id, ActiveFile::new(jar)?);
        }
        let active = self
            .active
            .get_mut(&segment.id)
            .expect("active writer established");
        active
            .writer
            .rows()
            .checked_add(1)
            .and_then(|rows| rows.checked_mul(row.len()))
            .ok_or(Error::Overflow)?;
        active.failed = true;
        for value in row {
            active.writer.append_column(Some(Ok(*value)))?;
            #[cfg(test)]
            test_sync::at(test_sync::Point::ColumnAppended)?;
        }
        active.failed = false;
        Ok(ordinal)
    }

    pub(crate) fn sync(&mut self) -> StorageResult<()> {
        for active in self.active.values_mut() {
            active.sync()?;
        }
        File::open(&self.files.directory)?.sync_all()?;
        Ok(())
    }

    /// Recover only against caller-published bounds. Drop buffered writers first:
    /// any best-effort buffer flush on drop must happen before trimming files.
    pub(crate) fn recover(
        &mut self,
        segment: &Segment,
        ranges: &BTreeMap<u64, Range<u64>>,
    ) -> StorageResult<()> {
        let files = self.files.clone();
        let _cleanup = files.lock_cleanup()?;
        self.active.remove(&segment.id);
        let starts = self.files.starts(segment)?;
        let mut keep = Vec::new();
        // Validate every required file before removing any unpublished files.
        for (&start, range) in ranges {
            let jar = self.files.configuration(segment, start)?;
            if jar.user_header().first_ordinal != range.start || end(&jar)? < range.end {
                return Err(Error::Corrupt("configuration lacks published rows"));
            }
            let rows = usize::try_from(range.end - range.start).map_err(|_| Error::Overflow)?;
            let count = rows.checked_mul(jar.columns()).ok_or(Error::Overflow)?;
            let offsets_len = (count as u64)
                .checked_add(1)
                .and_then(|n| n.checked_mul(8))
                .and_then(|n| n.checked_add(1))
                .ok_or(Error::Overflow)?;
            // Also bound the arithmetic used by the native consistency checker.
            jar.rows()
                .checked_mul(jar.columns())
                .and_then(|n| n.checked_mul(8))
                .and_then(|n| n.checked_add(9))
                .ok_or(Error::Overflow)?;
            let mut offsets = File::open(jar.offsets_path())?;
            let mut width = [0];
            offsets.read_exact(&mut width)?;
            if width != [8]
                || offsets.metadata()?.len() < offsets_len
                || read_offset(&mut offsets, 0)? != 0
            {
                return Err(Error::Corrupt("missing published offsets"));
            }
            let data_len = read_offset(&mut offsets, count)?;
            if fs::metadata(jar.data_path())?.len() < data_len {
                return Err(Error::Corrupt("missing published data"));
            }
            keep.push((jar, rows, offsets_len, data_len));
        }
        // A complete configuration must still belong to this deployment even
        // when it contains no published rows. A missing config is possible when
        // the process died during initial file creation.
        for &start in &starts {
            if !ranges.contains_key(&start)
                && self
                    .files
                    .path(segment, start)?
                    .with_extension("conf")
                    .exists()
            {
                self.files.configuration(segment, start)?;
            }
        }
        for (jar, rows, offsets_len, data_len) in keep {
            if jar.rows() != rows
                || fs::metadata(jar.offsets_path())?.len() != offsets_len
                || fs::metadata(jar.data_path())?.len() != data_len
            {
                let offsets = OpenOptions::new().write(true).open(jar.offsets_path())?;
                offsets.set_len(offsets_len)?;
                offsets.sync_all()?;
                let data = OpenOptions::new().write(true).open(jar.data_path())?;
                data.set_len(data_len)?;
                data.sync_all()?;
                // The offsets/data now describe exactly the published prefix.
                // Native healing may only reduce config.rows to that prefix.
                let active = ActiveFile::new(jar)?;
                if active.writer.rows() != rows {
                    return Err(Error::Corrupt("recovered row count mismatch"));
                }
                File::open(active.path.with_extension("conf"))?.sync_all()?;
                // Retain only the latest writer; historical readers stay uncached.
                if Some(&active.writer.user_header().first_commit) == ranges.keys().next_back() {
                    self.active.insert(segment.id, active);
                }
            }
        }
        for start in starts {
            let path = self.files.path(segment, start)?;
            let extensions: &[&str] = if ranges.contains_key(&start) {
                &["tmp"]
            } else {
                &["jar", "off", "conf", "tmp"]
            };
            for ext in extensions {
                match fs::remove_file(path.with_extension(ext)) {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        File::open(&self.files.directory)?.sync_all()?;
        Ok(())
    }

    pub(crate) fn get(
        &mut self,
        segment: &Segment,
        ordinal: u64,
        columns: &[usize],
    ) -> StorageResult<PayloadRow> {
        let limit = self.end_ordinal(segment)?;
        if ordinal >= limit {
            return Err(Error::NotFound);
        }
        if let Some(active) = self.active.get_mut(&segment.id)
            && ordinal >= active.writer.user_header().first_ordinal
        {
            if columns.is_empty() {
                return Ok(vec![]);
            }
            let start = active.writer.user_header().first_commit;
            active.sync()?;
            let jar = self.files.configuration(segment, start)?;
            return self
                .files
                .read_rows(jar, ordinal..ordinal + 1, columns, false)
                .map(|mut rows| rows.pop().unwrap_or_default());
        }
        for start in self.files.starts(segment)? {
            // The active file was already checked above. Older configurations
            // are inspected once, and the matched jar is reused for the read.
            if self
                .active
                .get(&segment.id)
                .is_some_and(|a| a.writer.user_header().first_commit == start)
            {
                continue;
            }
            let jar = self.files.configuration(segment, start)?;
            if (jar.user_header().first_ordinal..end(&jar)?).contains(&ordinal) {
                return self
                    .files
                    .read_rows(jar, ordinal..ordinal + 1, columns, false)
                    .map(|mut rows| rows.pop().unwrap_or_default());
            }
        }
        Err(Error::Corrupt("missing shard for physical ordinal"))
    }

    /// A commit's batch fits in exactly one commit-span file. Empty columns only
    /// validate physical range bounds, for metadata insertion conveniences.
    pub(crate) fn read_range(
        &mut self,
        segment: &Segment,
        commit: u64,
        range: Range<u64>,
        columns: &[usize],
    ) -> StorageResult<Vec<PayloadRow>> {
        let start = span_start(commit, self.files.span)?;
        if let Some(active) = self.active.get_mut(&segment.id)
            && active.writer.user_header().first_commit == start
        {
            active.check()?;
            if range.start > range.end
                || range.start < active.writer.user_header().first_ordinal
                || range.end > active.end()?
            {
                return Err(Error::Invalid("range exceeds physical shard bounds"));
            }
            if columns.is_empty() {
                return Ok(vec![]);
            }
            active.sync()?;
            let jar = self.files.configuration(segment, start)?;
            return self.files.read_rows(jar, range, columns, false);
        }
        let path = self.files.path(segment, start)?;
        if ![
            path.clone(),
            path.with_extension("conf"),
            path.with_extension("off"),
        ]
        .iter()
        .any(|p| p.exists())
        {
            return Err(Error::NotFound);
        }
        let jar = self.files.configuration(segment, start)?;
        self.files.read_rows(jar, range, columns, false)
    }

    pub(crate) fn truncate(&mut self, segment: &Segment, from: u64) -> StorageResult<()> {
        let files = self.files.clone();
        let _cleanup = files.lock_cleanup()?;
        let limit = self.end_ordinal(segment)?;
        if from > limit {
            return Err(Error::Invalid("truncate exceeds physical end"));
        }
        if from == limit {
            return Ok(());
        }
        if let Some(active) = self.active.get_mut(&segment.id) {
            active.sync()?;
        }
        // Validate the whole affected suffix before deleting anything. This is
        // physical tail removal, not retention pruning or database rollback.
        let mut remove = Vec::new();
        let mut retain = None;
        let mut found = from == 0;
        for start in self.files.starts(segment)? {
            let jar = self.files.configuration(segment, start)?;
            // Permit unfinished tails, but do not heal anything until the whole
            // suffix is validated. Removed files need no recovery; the retained
            // file is healed by ActiveFile::new before pruning its rows.
            inspect_file(&jar)?;
            let first = jar.user_header().first_ordinal;
            if first >= from {
                remove.push(jar);
                continue;
            }
            if end(&jar)? < from {
                return Err(Error::Corrupt("truncate boundary falls in missing data"));
            }
            let rows = usize::try_from(from - first).map_err(|_| Error::Overflow)?;
            validate_boundary(&jar, rows)?;
            retain = Some((jar, rows));
            found = true;
            break;
        }
        if !found {
            return Err(Error::Corrupt("missing truncate boundary"));
        }
        self.active.remove(&segment.id);
        for jar in remove {
            for path in [
                jar.data_path().to_owned(),
                jar.offsets_path(),
                jar.config_path(),
            ] {
                fs::remove_file(path)?;
            }
        }
        if let Some((jar, rows)) = retain {
            let excess = jar.rows() - rows;
            let mut active = ActiveFile::new(jar)?;
            active.writer.prune_rows(excess)?;
            active.sync()?;
            self.active.insert(segment.id, active);
        }
        File::open(&self.files.directory)?.sync_all()?;
        Ok(())
    }
}

fn end(jar: &NippyJar<FileHeader>) -> StorageResult<u64> {
    jar.user_header()
        .first_ordinal
        .checked_add(jar.rows() as u64)
        .ok_or(Error::Overflow)
}
fn check_range(jar: &NippyJar<FileHeader>, range: &Range<u64>) -> StorageResult<()> {
    if range.start > range.end
        || range.start < jar.user_header().first_ordinal
        || range.end > end(jar)?
    {
        return Err(Error::Invalid("range exceeds physical shard bounds"));
    }
    Ok(())
}

enum FileState {
    Consistent,
    UnfinishedTail,
}

/// Inspect required lengths and boundary offsets without changing files.
/// An unfinished tail extends beyond the configured rows and can be discarded.
/// Missing configured data must fail before NippyJar can heal by dropping rows.
fn inspect_file(jar: &NippyJar<FileHeader>) -> StorageResult<FileState> {
    if !jar.data_path().is_file() || !jar.offsets_path().is_file() {
        return Err(Error::Corrupt("missing shard file"));
    }
    let count = jar
        .rows()
        .checked_mul(jar.columns())
        .and_then(|n| n.checked_add(1))
        .ok_or(Error::Overflow)?;
    let expected = (count as u64)
        .checked_mul(8)
        .and_then(|n| n.checked_add(1))
        .ok_or(Error::Overflow)?;
    let mut offsets = File::open(jar.offsets_path())?;
    let offsets_len = offsets.metadata()?.len();
    if offsets_len < expected {
        return Err(Error::Corrupt("missing configured shard offsets"));
    }
    let mut width = [0];
    offsets.read_exact(&mut width)?;
    if width != [8] || read_offset(&mut offsets, 0)? != 0 {
        return Err(Error::Corrupt("inconsistent shard lengths or offsets"));
    }
    let data_len = fs::metadata(jar.data_path())?.len();
    let configured_end = read_offset(&mut offsets, count - 1)?;
    if configured_end > data_len {
        return Err(Error::Corrupt("missing configured shard data"));
    }
    if offsets_len > expected || data_len > configured_end {
        Ok(FileState::UnfinishedTail)
    } else {
        Ok(FileState::Consistent)
    }
}

// Concurrent appends may extend data/offsets before the atomic configuration
// replacement. Only the requested stable prefix must be complete. Interior
// column offsets are still read directly by NippyJar, without a separate scan.
fn validate_prefix(jar: &NippyJar<FileHeader>, exclusive_end: u64) -> StorageResult<()> {
    let rows = usize::try_from(exclusive_end - jar.user_header().first_ordinal)
        .map_err(|_| Error::Overflow)?;
    let count = rows.checked_mul(jar.columns()).ok_or(Error::Overflow)?;
    let required = (count as u64)
        .checked_add(1)
        .and_then(|n| n.checked_mul(8))
        .and_then(|n| n.checked_add(1))
        .ok_or(Error::Overflow)?;
    let mut offsets = File::open(jar.offsets_path())?;
    let mut width = [0];
    offsets.read_exact(&mut width)?;
    if width != [8]
        || offsets.metadata()?.len() < required
        || read_offset(&mut offsets, 0)? != 0
        || read_offset(&mut offsets, count)? > fs::metadata(jar.data_path())?.len()
    {
        return Err(Error::Corrupt("incomplete published file prefix"));
    }
    Ok(())
}

fn validate_boundary(jar: &NippyJar<FileHeader>, rows: usize) -> StorageResult<()> {
    let index = rows.checked_mul(jar.columns()).ok_or(Error::Overflow)?;
    let last = jar
        .rows()
        .checked_mul(jar.columns())
        .ok_or(Error::Overflow)?;
    let mut offsets = File::open(jar.offsets_path())?;
    let offset = read_offset(&mut offsets, index)?;
    // Surplus payload bytes cannot make a boundary outside configured data valid.
    if offset > read_offset(&mut offsets, last)? {
        return Err(Error::Corrupt("invalid truncation offset"));
    }
    Ok(())
}
/// The upstream cursor treats EOF as the end of its configuration's last
/// column. EOF may already include an uncommitted append. Use the native mapped
/// reader and codecs, but always end values at their stored terminal offset.
/// The caller holds the cleanup guard until this reader and all slices drop.
fn read_prefix_rows(
    jar: &NippyJar<FileHeader>,
    local: Range<usize>,
    columns: &[usize],
) -> StorageResult<Vec<PayloadRow>> {
    let reader = jar.open_data_reader()?;
    let mut rows = Vec::with_capacity(local.len());
    for row in local {
        let mut values = Vec::with_capacity(columns.len());
        for &column in columns {
            // Row/column arithmetic is bounded by validate_prefix above.
            let index = row * jar.columns() + column;
            let first = usize::try_from(reader.offset(index)?).map_err(|_| Error::Overflow)?;
            let last = usize::try_from(reader.offset(index + 1)?).map_err(|_| Error::Overflow)?;
            let bytes = reader.data(first..last);
            values.push(match jar.compressor() {
                Some(compression) => decompress_column(compression, bytes)?,
                None => bytes.to_vec(),
            });
        }
        rows.push(values);
    }
    Ok(rows)
}

fn decompress_column(compression: &Compressors, bytes: &[u8]) -> StorageResult<Vec<u8>> {
    if !matches!(compression, Compressors::Lz4(_)) {
        return Ok(compression.decompress(bytes)?);
    }
    // The pinned LZ4 `decompress` helper stops below 16x expansion, while valid
    // repeated-byte blocks can approach 255x. Use its native decompress_to with
    // a growing buffer. Each encoded length-extension byte contributes at most
    // 255 output bytes; the small slack covers token/base lengths. This bound
    // also prevents malformed blocks from causing an unbounded retry loop.
    let limit = bytes
        .len()
        .checked_mul(255)
        .and_then(|n| n.checked_add(32))
        .ok_or(Error::Overflow)?;
    let mut capacity = bytes.len().max(64).min(limit);
    let mut output = Vec::new();
    loop {
        output.reserve(capacity);
        match compression.decompress_to(bytes, &mut output) {
            Ok(()) => return Ok(output),
            Err(reth_nippy_jar::NippyJarError::OutputTooSmall) if capacity < limit => {
                output.clear();
                capacity = capacity.saturating_mul(2).min(limit);
            }
            Err(error) => return Err(error.into()),
        }
    }
}
fn read_offset(file: &mut File, index: usize) -> StorageResult<u64> {
    let position = (index as u64)
        .checked_mul(8)
        .and_then(|n| n.checked_add(1))
        .ok_or(Error::Overflow)?;
    file.seek(SeekFrom::Start(position))?;
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
#[path = "tests/file_readers.rs"]
mod reader_tests;

#[cfg(test)]
#[path = "tests/synchronization.rs"]
mod test_sync;
