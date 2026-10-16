use crate::{
    ImmutableDataCompression, ImmutableDataStorageError as Error, StorageResult,
    backend::{PayloadRow, span_start},
    storage::Segment,
};
use reth_nippy_jar::{NippyJar, NippyJarCursor, NippyJarWriter, compression::Compressors};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
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
    // or read silently consume it. Reopening can discard an unfinished file
    // tail; database publication and rollback still belong to the caller.
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

/// One owner per directory; only active writers are retained. Historical jar
/// configurations/readers live for one operation. The caller owns publication.
pub(crate) struct NippyJarStorage {
    directory: PathBuf,
    genesis_id: [u8; 32],
    span: u64,
    active: BTreeMap<u32, ActiveFile>,
    // Declared last so writer buffers are dropped before releasing ownership.
    _lock: File,
}
impl NippyJarStorage {
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
            active: BTreeMap::new(),
            _lock: lock,
        })
    }

    fn path(&self, segment: &Segment, start: u64) -> StorageResult<PathBuf> {
        let end = start.checked_add(self.span).ok_or(Error::Overflow)?;
        Ok(self.directory.join(format!(
            "segment-{}-{start}-{end}.jar",
            segment.schema.name()
        )))
    }

    /// Discover filenames only, sorting parsed u64 starts newest first. Include
    /// all three companions so a missing .conf cannot make an existing tail
    /// disappear or be overwritten.
    fn starts(&self, segment: &Segment) -> StorageResult<Vec<u64>> {
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
            if !["jar", "conf", "off"].contains(&ext) {
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

    pub(crate) fn end_ordinal(&self, segment: &Segment) -> StorageResult<u64> {
        if let Some(active) = self.active.get(&segment.id) {
            active.check()?;
            return active.end();
        }
        match self.starts(segment)?.first() {
            Some(&start) => end(&self.configuration(segment, start)?),
            None => Ok(0),
        }
    }

    pub(crate) fn append(
        &mut self,
        segment: &Segment,
        commit: u64,
        row: &[&[u8]],
    ) -> StorageResult<u64> {
        let start = span_start(commit, self.span)?;
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
        } else if let Some(&last) = self.starts(segment)?.first() {
            let jar = recover_file(self.configuration(segment, last)?)?;
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
                genesis_id: self.genesis_id,
                segment: segment.id,
                first_commit: start,
                first_ordinal: ordinal,
            };
            let jar = NippyJar::new(row.len(), &self.path(segment, start)?, header);
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
        }
        active.failed = false;
        Ok(ordinal)
    }

    pub(crate) fn sync(&mut self) -> StorageResult<()> {
        for active in self.active.values_mut() {
            active.sync()?;
        }
        File::open(&self.directory)?.sync_all()?;
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
            let jar = self.configuration(segment, start)?;
            return read_rows(jar, ordinal..ordinal + 1, columns)
                .map(|mut rows| rows.pop().unwrap_or_default());
        }
        for start in self.starts(segment)? {
            // The active file was already checked above. Older configurations
            // are inspected once, and the matched jar is reused for the read.
            if self
                .active
                .get(&segment.id)
                .is_some_and(|a| a.writer.user_header().first_commit == start)
            {
                continue;
            }
            let jar = self.configuration(segment, start)?;
            if (jar.user_header().first_ordinal..end(&jar)?).contains(&ordinal) {
                return read_rows(jar, ordinal..ordinal + 1, columns)
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
        let start = span_start(commit, self.span)?;
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
            let jar = self.configuration(segment, start)?;
            return read_rows(jar, range, columns);
        }
        let path = self.path(segment, start)?;
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
        let jar = self.configuration(segment, start)?;
        read_rows(jar, range, columns)
    }

    pub(crate) fn truncate(&mut self, segment: &Segment, from: u64) -> StorageResult<()> {
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
        for start in self.starts(segment)? {
            let jar = self.configuration(segment, start)?;
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
        File::open(&self.directory)?.sync_all()?;
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

/// Drop can flush buffered payloads without publishing their offsets/config.
/// Let NippyJar discard only that surplus, after checking the configured prefix.
/// Recovery is needed before reads too: the last column of the last row uses EOF.
fn recover_file(jar: NippyJar<FileHeader>) -> StorageResult<NippyJar<FileHeader>> {
    match inspect_file(&jar)? {
        FileState::Consistent => Ok(jar),
        FileState::UnfinishedTail => {
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
fn read_rows(
    jar: NippyJar<FileHeader>,
    range: Range<u64>,
    columns: &[usize],
) -> StorageResult<Vec<PayloadRow>> {
    check_range(&jar, &range)?;
    let jar = recover_file(jar)?;
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
    let mut sorted = columns.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mask = sorted.iter().fold(0usize, |mask, &i| mask | (1usize << i));
    let positions: Vec<_> = columns
        .iter()
        .map(|i| sorted.binary_search(i).expect("selected column"))
        .collect();
    if local.is_empty() {
        return Ok(vec![]);
    }
    // NippyJar reads column offsets directly through mmap. Basic file checks
    // above do not audit interior offsets; malformed ones may panic upstream.
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
