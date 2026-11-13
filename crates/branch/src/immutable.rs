//! Pending immutable rows are independent of cell overlays, seals and journals.
//! This module only reads metadata through the branch's validated snapshot.

use std::collections::{BTreeMap, BTreeSet};

use golemdb_immutable_data::{
    ImmutableDataConfig, ImmutableDataKey, ImmutableDataRow, ImmutableDataSchema,
    ImmutableDataStorageError as Error, StorageResult, metadata,
};
use golemdb_storage::ReadTransaction;

#[derive(Default)]
pub(crate) struct ImmutableSegments(BTreeMap<String, Segment>);

pub(crate) struct Segment {
    pub(crate) id: u32,
    pub(crate) schema: ImmutableDataSchema,
}

impl ImmutableSegments {
    pub(crate) fn by_id(&self, id: u32) -> &Segment {
        self.0
            .values()
            .find(|s| s.id == id)
            .expect("validated staged segment")
    }
    pub(crate) fn new(config: ImmutableDataConfig) -> StorageResult<Self> {
        let mut segments = BTreeMap::new();
        for (id, declaration) in config.segments_by_id()? {
            segments.insert(
                declaration.name.clone(),
                Segment {
                    id,
                    schema: declaration.schema()?,
                },
            );
        }
        Ok(Self(segments))
    }

    pub(crate) fn get(&self, name: &str) -> StorageResult<&Segment> {
        self.0.get(name).ok_or(Error::NotFound)
    }
}

/// All rows and reservations belong to one branch. Competing branches may stage
/// the same key and ordinal; ordinary head checks invalidate losing branches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ImmutableStaging {
    segments: BTreeMap<u32, SegmentBatch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegmentBatch {
    // Published segment end at branch origin; the first ordinal available for staging.
    pub(crate) first_ordinal: u64,
    pub(crate) rows: Vec<(Option<ImmutableDataKey>, ImmutableDataRow)>,
    keys: BTreeSet<ImmutableDataKey>,
}

impl SegmentBatch {
    pub(crate) fn end(&self) -> StorageResult<u64> {
        self.first_ordinal
            .checked_add(u64::try_from(self.rows.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)
    }
}

impl ImmutableStaging {
    pub(crate) fn batches(&self) -> impl Iterator<Item = (u32, &SegmentBatch)> {
        self.segments
            .iter()
            .filter(|(_, b)| !b.rows.is_empty())
            .map(|(&id, b)| (id, b))
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.segments.values().all(|batch| batch.rows.is_empty())
    }

    pub(crate) fn append(
        &mut self,
        origin: &impl ReadTransaction,
        commit: u64,
        segment: &Segment,
        key: Option<ImmutableDataKey>,
        row: ImmutableDataRow,
    ) -> StorageResult<u64> {
        segment.schema.validate_row(&row)?;
        let batch = self.segments.get(&segment.id);
        let first_ordinal = match batch {
            Some(batch) => batch.first_ordinal,
            None => metadata::end_ordinal_at(origin, segment.id, commit)?,
        };
        if let Some(key) = key {
            if batch.is_some_and(|batch| batch.keys.contains(&key)) {
                return Err(Error::AlreadyExists);
            }
            if let Some(ordinal) = metadata::ordinal_for_key(origin, segment.id, key)? {
                if ordinal >= first_ordinal {
                    return Err(Error::Corrupt("key exceeds published segment end"));
                }
                return Err(Error::AlreadyExists);
            }
        }
        let ordinal = match batch {
            Some(batch) => batch.end()?,
            None => first_ordinal,
        };
        ordinal.checked_add(1).ok_or(Error::Overflow)?;
        // All fallible validation and storage reads precede mutation.
        let batch = self
            .segments
            .entry(segment.id)
            .or_insert_with(|| SegmentBatch {
                first_ordinal,
                rows: Vec::new(),
                keys: BTreeSet::new(),
            });
        batch.rows.push((key, row));
        if let Some(key) = key {
            batch.keys.insert(key);
        }
        Ok(ordinal)
    }

    pub(crate) fn truncate(
        &mut self,
        origin: &impl ReadTransaction,
        commit: u64,
        segment: &Segment,
        from: u64,
    ) -> StorageResult<()> {
        let Some(batch) = self.segments.get_mut(&segment.id) else {
            return if from == metadata::end_ordinal_at(origin, segment.id, commit)? {
                Ok(())
            } else {
                Err(Error::Invalid(
                    "truncate must stay within the branch's staged tail",
                ))
            };
        };
        if from < batch.first_ordinal || from > batch.end()? {
            return Err(Error::Invalid(
                "truncate must stay within the branch's staged tail",
            ));
        }
        let keep = usize::try_from(from - batch.first_ordinal).map_err(|_| Error::Overflow)?;
        for (key, _) in batch.rows.drain(keep..) {
            if let Some(key) = key {
                batch.keys.remove(&key);
            }
        }
        Ok(())
    }
}
