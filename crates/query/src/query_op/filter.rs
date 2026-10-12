//! [`Filter`]: the filter tree a query evaluates, as the caller writes it.

use golemdb_cells::{CellName, CellValue};

/// A query's filter: plain data the caller builds, validated at admission.
///
/// Leaves are typed by their [`CellValue`], the same values records are
/// written with: a cell of another type under the same name is treated as
/// absent. The value's kind (field or attribute) plays no part in a lookup.
/// Variable-length `bytes` has no index term and is rejected at admission.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// The cell holds exactly `value`, with its type.
    Eq { field: CellName, value: CellValue },
    // In     { field: CellName, values: Vec<CellValue> },      // one type
    // Range  { field: CellName, lower: Bound<CellValue>, upper: Bound<CellValue> },
    // Prefix { field: CellName, prefix: CellValue },           // str, bytesN
    // Exists { field: CellName, ty: CellType },
    // And(Vec<Filter>),
    // Or(Vec<Filter>),
    // Not(Box<Filter>),
}
