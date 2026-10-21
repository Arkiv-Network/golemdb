use golemdb_record::{CreateEntry, PatchChange, PatchEntry};

use crate::{Budget, CellKind, CellName, CellNameRef, CellValue, RecordKey};

pub mod op {
    use super::*;
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Create {
        pub(super) entries: Vec<CreateEntry>,
    }
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Patch {
        pub(super) entries: Vec<PatchEntry>,
    }
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Get {
        pub(super) names: Option<Vec<CellName>>,
    }
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Delete;
}

/// A typed request with an explicit budget. Builder steps only collect input:
/// names, values, requested kinds and duplicates are preserved without validation
/// or normalization. Record execution admits the input after handle/snapshot
/// selection and returns any error with a receipt. There is no validation shortcut.
/// Budgets reach record execution but remain unenforced until metering is added.
///
/// Only patch operations support removals:
/// ```compile_fail
/// use golemdb_api::{Budget, RecordKey, RecordOp};
/// RecordOp::create(RecordKey([1; 32]), Budget::Limited(100)).remove("price");
/// ```
/// Reads cannot write cells:
/// ```compile_fail
/// use golemdb_api::{Budget, CellValue, RecordKey, RecordOp};
/// RecordOp::get(RecordKey([1; 32]), Budget::Limited(100)).field("price", CellValue::from_i32(50));
/// ```
/// Calls require the matching operation type:
/// ```compile_fail
/// use golemdb_api::{Api, BranchId, Budget, RecordKey, RecordOp};
/// fn wrong(api: &dyn Api, branch: BranchId) {
///     api.create(branch, RecordOp::delete(RecordKey([1; 32]), Budget::Limited(100)));
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct RecordOp<Op> {
    key: RecordKey,
    budget: Budget,
    operation: Op,
}

impl<Op> RecordOp<Op> {
    fn new(key: RecordKey, budget: Budget, operation: Op) -> Self {
        Self {
            key,
            budget,
            operation,
        }
    }
    pub fn record_key(&self) -> RecordKey {
        self.key
    }
    pub fn budget(&self) -> Budget {
        self.budget
    }
    pub fn max_cost(&self) -> Option<u64> {
        match self.budget {
            Budget::Limited(n) => Some(n),
            Budget::Unlimited => None,
        }
    }
}

impl RecordOp<op::Create> {
    pub fn create(key: RecordKey, budget: Budget) -> Self {
        Self::new(
            key,
            budget,
            op::Create {
                entries: Vec::new(),
            },
        )
    }
    pub fn attribute(self, name: &str, value: CellValue) -> Self {
        self.write(name, value, CellKind::Attribute)
    }
    pub fn field(self, name: &str, value: CellValue) -> Self {
        self.write(name, value, CellKind::Field)
    }
    fn write(mut self, name: &str, value: CellValue, kind: CellKind) -> Self {
        self.operation.entries.push(CreateEntry {
            name: CellNameRef::raw(name.as_bytes()).into(),
            value,
            kind,
        });
        self
    }
    /// Original entries in caller order, including invalid names and duplicates.
    /// The requested kind is separate from the unchanged value's kind.
    pub fn entries(&self) -> &[CreateEntry] {
        &self.operation.entries
    }
    pub(crate) fn into_create(self) -> (RecordKey, Budget, Vec<CreateEntry>) {
        (self.key, self.budget, self.operation.entries)
    }
}

impl RecordOp<op::Patch> {
    pub fn patch(key: RecordKey, budget: Budget) -> Self {
        Self::new(
            key,
            budget,
            op::Patch {
                entries: Vec::new(),
            },
        )
    }
    /// Replace an existing value, preserving its stored kind, but not its type.
    pub fn set(self, name: &str, value: CellValue) -> Self {
        self.edit(name, PatchChange::Set(value))
    }
    pub fn attribute(self, name: &str, value: CellValue) -> Self {
        self.edit(
            name,
            PatchChange::Write {
                value,
                kind: CellKind::Attribute,
            },
        )
    }
    pub fn field(self, name: &str, value: CellValue) -> Self {
        self.edit(
            name,
            PatchChange::Write {
                value,
                kind: CellKind::Field,
            },
        )
    }
    pub fn remove(self, name: &str) -> Self {
        self.edit(name, PatchChange::Remove)
    }
    fn edit(mut self, name: &str, change: PatchChange) -> Self {
        self.operation.entries.push(PatchEntry {
            name: CellNameRef::raw(name.as_bytes()).into(),
            change,
        });
        self
    }
    /// Original requested changes in caller order, including duplicates.
    pub fn entries(&self) -> &[PatchEntry] {
        &self.operation.entries
    }
    pub(crate) fn into_patch(self) -> (RecordKey, Budget, Vec<PatchEntry>) {
        (self.key, self.budget, self.operation.entries)
    }
}

impl RecordOp<op::Get> {
    pub fn get(key: RecordKey, budget: Budget) -> Self {
        Self::new(key, budget, op::Get { names: None })
    }
    /// Retain raw projection names in caller order, including duplicates.
    /// Execution normalizes them after selecting the target. Empty means existence only.
    pub fn only<I, N>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = N>,
        N: AsRef<[u8]>,
    {
        self.operation.names = Some(
            names
                .into_iter()
                .map(|name| CellNameRef::raw(name.as_ref()).into())
                .collect(),
        );
        self
    }
    pub fn names(&self) -> Option<&[CellName]> {
        self.operation.names.as_deref()
    }
}
impl RecordOp<op::Delete> {
    pub fn delete(key: RecordKey, budget: Budget) -> Self {
        Self::new(key, budget, op::Delete)
    }
}
