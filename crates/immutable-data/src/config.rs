//! Genesis declarations and the stable `#immutableDataSegments` payload.

use std::collections::BTreeSet;

use golemdb_cells::{CellNameError, CellNameRef, CellType, FloatWidth, Width};
use serde::{Deserialize, Serialize};

use crate::{
    ImmutableDataColumn, ImmutableDataError, ImmutableDataSchema, schema::validate_segment_name,
};

/// Fixed engine limit for the 64-bit NippyJar projection mask. Backends must
/// additionally check their platform's mask width before opening files.
pub const MAX_COLUMNS: usize = 64;
/// Maximum number of declared segments in the current genesis format.
pub const MAX_SEGMENTS: usize = MAX_COLUMNS;
/// The schema parameter plus its cell tag must fit the 16 KiB engine row budget.
/// This is a configuration limit, not a limit on immutable row payloads.
pub const MAX_SCHEMA_BYTES: usize = 16 * 1024 - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImmutableDataCompression {
    None,
    Lz4,
    /// Ordinary Zstd, without dictionary training.
    Zstd,
}

impl ImmutableDataCompression {
    fn id(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Lz4 => 1,
            Self::Zstd => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableDataSegment {
    /// Filesystem-safe name matching `[a-z][a-z0-9_]{0,63}`; never normalized.
    pub name: String,
    /// Order is significant and is preserved in the committed declaration.
    pub columns: Vec<ImmutableDataColumn>,
    pub compression: ImmutableDataCompression,
}

impl ImmutableDataSegment {
    pub fn schema(&self) -> crate::Result<ImmutableDataSchema> {
        ImmutableDataSchema::new(self.name.clone(), self.columns.clone())
    }
}

/// Schema and file settings for immutable storage. The API's genesis configuration
/// wraps this in Option; None disables the feature without a placeholder shard span.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableDataConfig {
    /// Number of commits per physical file span, independently for each segment.
    pub shard_span: u64,
    pub segments: Vec<ImmutableDataSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImmutableDataConfigError {
    #[error("immutable_data.shard_span must be positive")]
    InvalidShardSpan,
    #[error("at most {MAX_SEGMENTS} immutable segments are supported, got {0}")]
    TooManySegments(usize),
    #[error("segment {segment:?} needs 1..={MAX_COLUMNS} columns, got {actual}")]
    ColumnCount { segment: String, actual: usize },
    #[error("invalid immutable-data name {name:?}: {source}")]
    InvalidName {
        name: String,
        #[source]
        source: CellNameError,
    },
    #[error("duplicate immutable segment {0:?}")]
    DuplicateSegment(String),
    #[error(transparent)]
    Schema(#[from] ImmutableDataError),
    #[error("encoded immutable schema needs {size} bytes; maximum is {MAX_SCHEMA_BYTES}")]
    SchemaTooLarge { size: usize },
}

impl ImmutableDataConfig {
    pub const fn new(shard_span: u64, segments: Vec<ImmutableDataSegment>) -> Self {
        Self {
            shard_span,
            segments,
        }
    }

    pub fn validate(&self) -> Result<(), ImmutableDataConfigError> {
        if self.shard_span == 0 {
            return Err(ImmutableDataConfigError::InvalidShardSpan);
        }
        if self.segments.len() > MAX_SEGMENTS {
            return Err(ImmutableDataConfigError::TooManySegments(
                self.segments.len(),
            ));
        }
        let mut names = BTreeSet::new();
        let mut size = 1 + 4; // format version and segment count
        for segment in &self.segments {
            validate_segment_name(&segment.name)?;
            if !names.insert(&segment.name) {
                return Err(ImmutableDataConfigError::DuplicateSegment(
                    segment.name.clone(),
                ));
            }
            if segment.columns.is_empty() || segment.columns.len() > MAX_COLUMNS {
                return Err(ImmutableDataConfigError::ColumnCount {
                    segment: segment.name.clone(),
                    actual: segment.columns.len(),
                });
            }
            size += 4 + segment.name.len() + 1 + 4;
            for column in &segment.columns {
                validate_column_name(&column.name)?;
                size += 4 + column.name.len() + 1;
            }
            segment.schema()?;
        }
        // Name lengths and counts above bound this arithmetic even on 32-bit hosts.
        if size > MAX_SCHEMA_BYTES {
            return Err(ImmutableDataConfigError::SchemaTooLarge { size });
        }
        Ok(())
    }

    /// IDs are zero-based positions in bytewise segment-name order. User segment
    /// IDs do not depend on YAML order; column order remains significant.
    pub fn segments_by_id(
        &self,
    ) -> Result<Vec<(u32, &ImmutableDataSegment)>, ImmutableDataConfigError> {
        self.validate()?;
        Ok(self
            .sorted_segments()
            .into_iter()
            .enumerate()
            .map(|(id, segment)| (id as u32, segment))
            .collect())
    }

    /// Deterministic payload for `#params.#immutableDataSegments`, excluding its
    /// cell tag. Format: version:u8=1, count:u32, then sorted segments encoded as
    /// name_len:u32, name bytes, compression:u8, column_count:u32, followed by
    /// ordered columns (name_len:u32, name bytes, CellType::id():u8).
    /// All integers are big endian. Shard span is a separate u64 parameter cell.
    /// The opener compares this exact encoding on reopen; no YAML is persisted.
    pub fn encode_segments(&self) -> Result<Vec<u8>, ImmutableDataConfigError> {
        self.validate()?;
        let mut bytes = vec![1];
        bytes.extend_from_slice(&(self.segments.len() as u32).to_be_bytes());
        for segment in self.sorted_segments() {
            encode_name(&mut bytes, &segment.name);
            bytes.push(segment.compression.id());
            bytes.extend_from_slice(&(segment.columns.len() as u32).to_be_bytes());
            for column in &segment.columns {
                encode_name(&mut bytes, &column.name);
                bytes.push(column.value_type.id());
            }
        }
        Ok(bytes)
    }

    fn sorted_segments(&self) -> Vec<&ImmutableDataSegment> {
        let mut segments: Vec<_> = self.segments.iter().collect();
        segments.sort_by(|a, b| a.name.cmp(&b.name));
        segments
    }
}

fn validate_column_name(name: &str) -> Result<(), ImmutableDataConfigError> {
    // Reuse the user-name grammar, independently of deployment cell-name limits.
    CellNameRef::parse_user(name.as_bytes(), MAX_SCHEMA_BYTES)
        .map(|_| ())
        .map_err(|source| ImmutableDataConfigError::InvalidName {
            name: name.into(),
            source,
        })
}

fn encode_name(bytes: &mut Vec<u8>, name: &str) {
    bytes.extend_from_slice(&(name.len() as u32).to_be_bytes());
    bytes.extend_from_slice(name.as_bytes());
}

pub(crate) fn bytes_type() -> CellType {
    CellType::Bytes
}

/// YAML spelling only; payload validation and encoding stay in golemdb-cells.
pub(crate) mod column_type {
    use super::*;
    use serde::{Deserializer, Serializer, de::Error};

    const TYPES: &[(&str, CellType)] = &[
        ("bool", CellType::Bool),
        ("str", CellType::Str),
        ("bytes", CellType::Bytes),
        ("bytes20", CellType::Bytes20),
        ("bytes4", CellType::FixedBytes(Width::W4)),
        ("bytes8", CellType::FixedBytes(Width::W8)),
        ("bytes16", CellType::FixedBytes(Width::W16)),
        ("bytes32", CellType::FixedBytes(Width::W32)),
        ("u32", CellType::Uint(Width::W4)),
        ("u64", CellType::Uint(Width::W8)),
        ("u128", CellType::Uint(Width::W16)),
        ("u256", CellType::Uint(Width::W32)),
        ("i32", CellType::Int(Width::W4)),
        ("i64", CellType::Int(Width::W8)),
        ("i128", CellType::Int(Width::W16)),
        ("i256", CellType::Int(Width::W32)),
        ("dec32", CellType::Decimal(Width::W4)),
        ("dec64", CellType::Decimal(Width::W8)),
        ("dec128", CellType::Decimal(Width::W16)),
        ("dec256", CellType::Decimal(Width::W32)),
        ("f32", CellType::Float(FloatWidth::F32)),
        ("f64", CellType::Float(FloatWidth::F64)),
        ("date32", CellType::Date32),
        ("timestamp64", CellType::Timestamp64),
    ];

    pub fn serialize<S: Serializer>(value: &CellType, serializer: S) -> Result<S::Ok, S::Error> {
        let name = TYPES
            .iter()
            .find(|(_, ty)| ty == value)
            .ok_or_else(|| serde::ser::Error::custom("unsupported immutable column type"))?
            .0;
        serializer.serialize_str(name)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<CellType, D::Error> {
        let name = String::deserialize(deserializer)?;
        TYPES
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, ty)| *ty)
            .ok_or_else(|| D::Error::custom(format!("unsupported immutable column type {name:?}")))
    }
}
