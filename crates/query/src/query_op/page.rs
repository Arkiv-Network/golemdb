//! [`QueryPage`]: one page of a query's result, and the [`Cursor`] to the next.

use golemdb_branch::CommitId;
use golemdb_record::Record;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPage {
    /// The page's records, projected, in query order.
    pub records: Vec<Record>,
    /// The size of the whole match set, when the query asked for it.
    pub total_matched: Option<u64>,
    /// The commit the page was evaluated at.
    pub evaluated_at: CommitId,
    // /// Where the next page starts; `None` when nothing follows.
    // pub next: Option<Cursor>,
}

