//! [`Filter`]: the filter tree a query evaluates, as the caller writes it.

use golemdb_cells::CellName;

use super::{Bounds, EqValue, ExistsType, InValues};

/// A query's filter: plain data the caller builds, validated at admission.
///
/// A tree of `And`, `Or` and `Not` nodes over leaves. The written tree is
/// the execution plan: nodes are evaluated depth-first, children left to
/// right, so a condition shared by several branches is written, and read,
/// once: `(a ∨ b) ∧ c` reads `c` a single time. How deep the tree nests is
/// capped by the `max_query_depth` of the [`Queries`](crate::Queries) it
/// runs on: a node more `And` / `Or` / `Not` levels deep fails as
/// `LimitExceeded` at admission.
///
/// Leaves are typed by their values, the same `CellValue`s records are
/// written with: a cell of another type under the same name is treated as
/// absent. The value's kind (field or attribute) plays no part in a lookup.
/// An `Eq`'s value, an `In`'s values, a `Range`'s bounds and an `Exists`'
/// type are of one indexable type by construction ([`EqValue`],
/// [`InValues`], [`Bounds`], [`ExistsType`]), so only a field name is
/// checked at admission. An `Exists` with no type is the one leaf that
/// matches a cell of any type.
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
    /// The record has the cell, of type `ty`; with no type, of any type.
    /// Like every leaf, it sees attribute cells only: a field is not
    /// indexed.
    Exists {
        field: CellName,
        ty: Option<ExistsType>,
    },
    /// Every child matches: the intersection of the children's match sets,
    /// less the match set under each `Not` child. Children are evaluated in
    /// the written order, and once no record is left, the remaining children
    /// are skipped and not read. Every child after the first, and everything
    /// under it, reads only the index containers in the regions (blocks of
    /// 65 536 record IDs) of the records left so far, so a selective first
    /// child makes the rest cheaper. At least one child.
    And(Vec<Filter>),
    /// Some child matches: the union of the children's match sets. Every
    /// child is evaluated, in the written order. At least one child.
    Or(Vec<Filter>),
    /// The child does not match. A record with no attribute of the leaf's
    /// name, or one of another type, matches a `Not` of that leaf, as does
    /// one holding the name as a field: fields are never indexed, so filters
    /// never see them. `Not` of `color = "red"` keeps the records with no
    /// `color` attribute, a `color` field of `"red"` included. A `Not`
    /// after an `And`'s first child subtracts its child's match set from
    /// the running set. Anywhere else, standalone, it subtracts it from the
    /// set of all live records, read first from the index's `#is_live` term: an
    /// extra read whose size grows with the number of live records, one
    /// container per 65 536 of them. So place a `Not` after a selective
    /// positive child of an `And` where the query allows it.
    Not(Box<Filter>),
}
