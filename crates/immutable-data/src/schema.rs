use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use golemdb_cells::{CellType, CellValueRef};

use crate::{ImmutableDataError, ImmutableDataReadOptions, ImmutableDataRow, Result};

/// Segment names become filenames, so use one filesystem-independent spelling.
pub(crate) fn validate_segment_name(name: &str) -> Result<()> {
    let Some((&first, rest)) = name.as_bytes().split_first() else {
        return Err(ImmutableDataError::EmptySegmentName);
    };
    if name.len() > 64
        || !first.is_ascii_lowercase()
        || !rest
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
    {
        return Err(ImmutableDataError::InvalidSegmentName { name: name.into() });
    }
    Ok(())
}

/// A column's logical type is fixed by its segment declaration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableDataColumn {
    pub name: String,
    #[serde(
        rename = "type",
        default = "crate::config::bytes_type",
        with = "crate::config::column_type"
    )]
    pub value_type: CellType,
}

impl ImmutableDataColumn {
    /// A named column defaults to opaque bytes, with no cell byte-length cap.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value_type: CellType::Bytes,
        }
    }

    pub const fn with_type(mut self, value_type: CellType) -> Self {
        self.value_type = value_type;
        self
    }
}

/// Validated segment name and ordered, uniquely named columns.
///
/// Segment names match `[a-z][a-z0-9_]{0,63}`. The selected backend and genesis
/// configuration additionally validate column-count and column-name limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableDataSchema {
    name: String,
    columns: Arc<[ImmutableDataColumn]>,
}

impl ImmutableDataSchema {
    pub fn new(name: impl Into<String>, columns: Vec<ImmutableDataColumn>) -> Result<Self> {
        let name = name.into();
        validate_segment_name(&name)?;
        if columns.is_empty() {
            return Err(ImmutableDataError::EmptySchema);
        }
        let mut names = BTreeSet::new();
        for (column, declaration) in columns.iter().enumerate() {
            if declaration.name.is_empty() {
                return Err(ImmutableDataError::EmptyColumnName { column });
            }
            if !names.insert(declaration.name.as_str()) {
                return Err(ImmutableDataError::DuplicateColumn {
                    name: declaration.name.clone(),
                });
            }
        }
        Ok(Self {
            name,
            columns: columns.into(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn columns(&self) -> &[ImmutableDataColumn] {
        &self.columns
    }

    /// Check the complete append row, without coercing numeric types or treating
    /// a bytes declaration as a wildcard. CellValue already validates encoding.
    pub fn validate_row(&self, row: &ImmutableDataRow) -> Result<()> {
        if row.values().len() != self.columns.len() {
            return Err(ImmutableDataError::RowArity {
                expected: self.columns.len(),
                actual: row.values().len(),
            });
        }
        for (column, (declaration, value)) in self.columns.iter().zip(row.values()).enumerate() {
            if declaration.value_type != value.cell_type() {
                return Err(ImmutableDataError::TypeMismatch {
                    column,
                    expected: declaration.value_type,
                    actual: value.cell_type(),
                });
            }
        }
        Ok(())
    }

    /// Resolve once before reading payloads. Indices follow caller order, not
    /// schema order. The projection and decoded rows share owned layout metadata,
    /// so rows remain usable after the schema and projection are dropped.
    pub fn projection(
        &self,
        options: &ImmutableDataReadOptions,
    ) -> Result<ImmutableDataProjection> {
        let indices: Vec<usize> = match &options.columns {
            None => (0..self.columns.len()).collect(),
            Some(names) => {
                if names.is_empty() {
                    return Err(ImmutableDataError::EmptyProjection);
                }
                let mut seen = BTreeSet::new();
                let mut indices = Vec::with_capacity(names.len());
                for name in names {
                    let index = self
                        .columns
                        .iter()
                        .position(|column| column.name == *name)
                        .ok_or_else(|| ImmutableDataError::UnknownColumn { name: name.clone() })?;
                    if !seen.insert(index) {
                        return Err(ImmutableDataError::DuplicateProjection { name: name.clone() });
                    }
                    indices.push(index);
                }
                indices
            }
        };
        let positions = indices
            .iter()
            .enumerate()
            .map(|(position, &column)| (self.columns[column].name.clone(), position))
            .collect();
        Ok(ImmutableDataProjection {
            layout: Arc::new(RowLayout {
                columns: Arc::clone(&self.columns),
                indices,
                positions,
            }),
        })
    }

    /// Decode one payload per schema column. Payloads exclude cell metadata.
    pub fn decode_row(&self, payloads: &[&[u8]]) -> Result<ImmutableDataRow> {
        self.projection(&ImmutableDataReadOptions::all())?
            .decode(payloads)
    }
}

/// Resolved column selection. A backend fetches payloads for `indices()` and
/// supplies them to `decode` in that order; unselected columns are not decoded.
#[derive(Debug, Clone)]
pub struct ImmutableDataProjection {
    layout: Arc<RowLayout>,
}

/// Shared by every row decoded with one projection. Positions are local to the
/// returned row; indices map those positions back to the complete schema.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RowLayout {
    columns: Arc<[ImmutableDataColumn]>,
    indices: Vec<usize>,
    positions: BTreeMap<String, usize>,
}

impl RowLayout {
    pub(crate) fn column_index(&self, name: &str) -> Result<usize> {
        if let Some(&position) = self.positions.get(name) {
            return Ok(position);
        }
        if self.columns.iter().any(|column| column.name == name) {
            Err(ImmutableDataError::ColumnNotSelected { name: name.into() })
        } else {
            Err(ImmutableDataError::UnknownColumn { name: name.into() })
        }
    }

    pub(crate) fn column_name(&self, position: usize) -> &str {
        &self.columns[self.indices[position]].name
    }
}

impl ImmutableDataProjection {
    pub fn indices(&self) -> &[usize] {
        &self.layout.indices
    }

    pub fn decode(&self, payloads: &[&[u8]]) -> Result<ImmutableDataRow> {
        if payloads.len() != self.layout.indices.len() {
            return Err(ImmutableDataError::PayloadArity {
                expected: self.layout.indices.len(),
                actual: payloads.len(),
            });
        }
        let values = self
            .layout
            .indices
            .iter()
            .zip(payloads)
            .map(|(&column, payload)| {
                let declaration = &self.layout.columns[column];
                CellValueRef::new(declaration.value_type, payload, false)
                    .map(Into::into)
                    .map_err(|source| ImmutableDataError::InvalidPayload {
                        column,
                        name: declaration.name.clone(),
                        source,
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ImmutableDataRow::from_decoded(
            values,
            Arc::clone(&self.layout),
        ))
    }
}
