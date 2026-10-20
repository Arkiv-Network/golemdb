//! [`Filter`]: the filter tree a query evaluates, as the caller writes it.

use golemdb_cells::CellName;

use super::{Bounds, EqValue, InValues};

/// A query's filter: plain data the caller builds, validated at admission.
///
/// Leaves are typed by their values, the same `CellValue`s records are
/// written with: a cell of another type under the same name is treated as
/// absent. The value's kind (field or attribute) plays no part in a lookup.
/// An `Eq`'s value, an `In`'s values and a `Range`'s bounds are of one
/// indexable type by construction ([`EqValue`], [`InValues`], [`Bounds`]),
/// so only a field name is checked at admission.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// The cell holds exactly `value`, with its type.
    Eq { field: CellName, value: EqValue },
    /// The cell holds one of `values`: of one indexable type, distinct and
    /// ascending by construction. No value matches nothing.
    In { field: CellName, values: InValues },
    /// The cell's value, of the bounds' type, lies between the bounds, in
    /// that type's order: numeric for numbers and dates, false before true,
    /// byte order for strings (UTF-8) and fixed bytes. Two unbounded sides
    /// match every cell of that type.
    Range { field: CellName, bounds: Bounds },
    /// The cell holds a `str` that starts with `prefix`: byte by byte in
    /// UTF-8, which is also char by char. A cell of another type never
    /// matches. The empty string matches every `str` cell.
    Prefix { field: CellName, prefix: String },
    // Exists { field: CellName, ty: CellType },
    // And(Vec<Filter>),
    // Or(Vec<Filter>),
    // Not(Box<Filter>),
}
