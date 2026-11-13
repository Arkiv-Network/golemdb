//! Plain query execution input. Budgets are passed separately to [`crate::Queries`].

use golemdb_cells::CellName;

use crate::{Filter, SortKey};

/// A filtered, optionally sorted and projected, paged read.
///
/// Plain execution input: admission checks limits, the filter and sort keys
/// after snapshot selection. The caller passes a budget separately to
/// [`Queries::query`](crate::Queries::query) or its report variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryRequest {
    pub filter: Filter,
    /// Empty: record ID ascending.
    pub sort: Vec<SortKey>,
    /// `None` selects full records; `Some([])` selects no cells.
    pub projection: Option<Vec<CellName>>,
    /// The most records the page holds, the first in query order. `None`:
    /// up to the `max_query_limit` of the [`Queries`](crate::Queries) it runs
    /// on.
    pub limit: Option<u64>,
    /// How many matches, in query order, come before the page. `None`: 0.
    pub offset: Option<u64>,
    pub with_total: bool,
}
