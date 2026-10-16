//! [`Filter`]: the filter tree a query evaluates, as the caller writes it.

use std::ops::Bound;

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
    /// The cell holds one of `values`. The values must be of one type; a
    /// value given twice is read once, and no value matches nothing.
    In {
        field: CellName,
        values: Vec<CellValue>,
    },
    /// The cell's value lies between `lower` and `upper`, in its type's
    /// order: numeric for numbers and dates, false before true, byte order
    /// for strings (UTF-8) and fixed bytes. At least one side is bounded.
    Range {
        field: CellName,
        lower: Bound<CellValue>,
        upper: Bound<CellValue>,
    },
    /// The cell holds a `str` that starts with `prefix`: byte by byte in
    /// UTF-8, which is also char by char. A cell of another type never
    /// matches. The empty string matches every `str` cell.
    Prefix { field: CellName, prefix: String },
    // Exists { field: CellName, ty: CellType },
    // And(Vec<Filter>),
    // Or(Vec<Filter>),
    // Not(Box<Filter>),
}
