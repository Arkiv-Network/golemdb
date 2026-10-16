use std::collections::{BTreeMap, btree_map::Entry};

use golemdb_record::{CellPatch, RecordCells, RecordPatch};

use crate::{ApiError, CellKind, CellName, CellNameRef, CellValue, RecordKey, Result};

/// Operation types used by [`RecordOp`] and the CRUD methods of [`crate::Api`].
/// Their contents are built through RecordOp constructors and helpers.
pub mod op {
    use super::*;

    /// Create a caller-keyed record, optionally with user cells.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Create {
        pub(super) cells: RecordCells,
    }

    /// Set or remove user cells on an existing record.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Patch {
        pub(super) patch: RecordPatch,
    }

    /// Read all cells or an explicit projection.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Get {
        pub(super) names: Option<Vec<CellName>>,
    }

    /// Delete a record and its binding.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Delete;
}

/// An owned, typed CRUD request. The key is required at construction; branch or
/// read target is chosen when executing the request through [`crate::Api`].
/// Requests can be cloned and inspected by API implementations and mocks.
///
/// Write helpers accept explicit [`CellValue`]s and immediately reject invalid
/// names, incompatible kinds, and duplicate names. Deployment limits and record
/// existence are checked at execution. Empty creates and patches are valid.
///
/// Only patch operations support removals:
/// ```compile_fail
/// use golemdb_api::{RecordKey, RecordOp};
/// RecordOp::create(RecordKey([1; 32])).remove("price");
/// ```
/// Read operations cannot write cells:
/// ```compile_fail
/// use golemdb_api::{CellValue, RecordKey, RecordOp};
/// RecordOp::get(RecordKey([1; 32])).field("price", CellValue::from_i32(50));
/// ```
/// CRUD methods require the matching operation type:
/// ```compile_fail
/// use golemdb_api::{Api, BranchId, RecordKey, RecordOp};
/// fn wrong_operation(api: &dyn Api, branch: BranchId) {
///     api.create(branch, RecordOp::delete(RecordKey([1; 32])));
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordOp<Op> {
    key: RecordKey,
    operation: Op,
}

impl<Op> RecordOp<Op> {
    /// The caller-provided key, shared by all operation types.
    pub fn record_key(&self) -> RecordKey {
        self.key
    }
}

impl RecordOp<op::Create> {
    /// Create a record with its system identity and initially no user cells.
    pub fn create(key: RecordKey) -> Self {
        Self {
            key,
            operation: op::Create {
                cells: RecordCells::new(),
            },
        }
    }

    /// Insert an already-constructed value, retaining its kind and type.
    /// Repeated names are errors rather than implicit overwrites.
    pub fn insert(self, name: &str, value: CellValue) -> Result<Self> {
        self.write("insert", name, value)
    }

    fn write(mut self, step: &str, name: &str, value: CellValue) -> Result<Self> {
        insert_named(&mut self.operation.cells, step, name.as_bytes(), value)?;
        Ok(self)
    }

    pub fn attribute(self, name: &str, value: CellValue) -> Result<Self> {
        self.write(
            "attribute",
            name,
            value
                .with_kind(CellKind::Attribute)
                .map_err(|error| located("attribute", name.as_bytes(), error.into()))?,
        )
    }

    pub fn field(self, name: &str, value: CellValue) -> Result<Self> {
        self.write(
            "field",
            name,
            value
                .with_kind(CellKind::Field)
                .map_err(|error| located("field", name.as_bytes(), error.into()))?,
        )
    }

    /// Look up a supplied value without exposing the record layer's map type.
    pub fn value(&self, name: impl AsRef<[u8]>) -> Option<&CellValue> {
        self.operation.cells.get(name.as_ref())
    }

    /// Supplied cells in byte order, for API adapters and mocks.
    pub fn cells(&self) -> impl ExactSizeIterator<Item = (&CellName, &CellValue)> {
        self.operation.cells.iter()
    }

    pub(crate) fn into_create(self) -> (RecordKey, RecordCells) {
        (self.key, self.operation.cells)
    }
}

impl RecordOp<op::Patch> {
    /// Start an empty patch. Removing every user cell preserves record identity.
    pub fn patch(key: RecordKey) -> Self {
        Self {
            key,
            operation: op::Patch {
                patch: RecordPatch::new(),
            },
        }
    }

    /// Set a value while retaining its existing kind and type.
    pub fn set(self, name: &str, value: CellValue) -> Result<Self> {
        self.edit("set", name, CellPatch::Set(value))
    }

    pub fn attribute(self, name: &str, value: CellValue) -> Result<Self> {
        self.edit(
            "attribute",
            name,
            CellPatch::Set(
                value
                    .with_kind(CellKind::Attribute)
                    .map_err(|error| located("attribute", name.as_bytes(), error.into()))?,
            ),
        )
    }

    pub fn field(self, name: &str, value: CellValue) -> Result<Self> {
        self.edit(
            "field",
            name,
            CellPatch::Set(
                value
                    .with_kind(CellKind::Field)
                    .map_err(|error| located("field", name.as_bytes(), error.into()))?,
            ),
        )
    }

    pub fn remove(self, name: &str) -> Result<Self> {
        self.edit("remove", name, CellPatch::Remove)
    }

    fn edit(mut self, step: &str, name: &str, change: CellPatch) -> Result<Self> {
        insert_named(&mut self.operation.patch, step, name.as_bytes(), change)?;
        Ok(self)
    }

    /// A value supplied by this patch, absent for removals and unmentioned cells.
    pub fn value(&self, name: impl AsRef<[u8]>) -> Option<&CellValue> {
        match self.operation.patch.get(name.as_ref()) {
            Some(CellPatch::Set(value)) => Some(value),
            _ => None,
        }
    }

    /// Whether this patch explicitly removes the named cell.
    pub fn removes(&self, name: impl AsRef<[u8]>) -> bool {
        matches!(
            self.operation.patch.get(name.as_ref()),
            Some(CellPatch::Remove)
        )
    }

    /// Changes in byte order. Some(value) sets a cell; None removes it.
    pub fn changes(&self) -> impl ExactSizeIterator<Item = (&CellName, Option<&CellValue>)> {
        self.operation.patch.iter().map(|(name, change)| {
            (
                name,
                match change {
                    CellPatch::Set(value) => Some(value),
                    CellPatch::Remove => None,
                },
            )
        })
    }

    pub(crate) fn into_patch(self) -> (RecordKey, RecordPatch) {
        (self.key, self.operation.patch)
    }
}

impl RecordOp<op::Get> {
    /// Read the complete record, including #key.
    pub fn get(key: RecordKey) -> Self {
        Self {
            key,
            operation: op::Get { names: None },
        }
    }

    /// Replace the projection with text or raw byte names, deduplicated in byte
    /// order. Reserved and binary names are readable. An empty selection returns
    /// no cells while still checking record existence.
    pub fn only<I, N>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = N>,
        N: AsRef<[u8]>,
    {
        self.operation.names = Some(CellName::projection(names));
        self
    }

    /// None selects the full record; Some([]) selects no cells.
    pub fn names(&self) -> Option<&[CellName]> {
        self.operation.names.as_deref()
    }
}

impl RecordOp<op::Delete> {
    /// Delete the record and its binding without reclaiming its internal ID.
    pub fn delete(key: RecordKey) -> Self {
        Self {
            key,
            operation: op::Delete,
        }
    }
}

fn validate_name(step: &str, name: &[u8]) -> Result<CellName> {
    // The builder has no database configuration. Validate only grammar; record
    // admission checks #maxCellNameLen against the deployment's actual value.
    CellNameRef::parse_user(name, usize::MAX)
        .map(Into::into)
        .map_err(|error| located(step, name, error.into()))
}

fn insert_named<T>(
    values: &mut BTreeMap<CellName, T>,
    step: &str,
    name: &[u8],
    value: T,
) -> Result<()> {
    match values.entry(validate_name(step, name)?) {
        Entry::Vacant(entry) => {
            entry.insert(value);
            Ok(())
        }
        Entry::Occupied(_) => Err(located(
            step,
            name,
            ApiError::invalid_argument("duplicate cell name"),
        )),
    }
}

fn located(step: &str, name: &[u8], error: ApiError) -> ApiError {
    match error {
        ApiError::InvalidArgument { message, source } => {
            let name = match std::str::from_utf8(name) {
                Ok(text) => format!("{text:?}"),
                Err(_) => format!("{name:?}"),
            };
            ApiError::InvalidArgument {
                message: format!("{step}({name}): {message}"),
                source,
            }
        }
        error => error,
    }
}
