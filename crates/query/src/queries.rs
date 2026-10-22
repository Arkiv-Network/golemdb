//! [`Queries`]: `query` and `count` over committed state.
//!
//! Checks run in this order:
//!
//! 1. Open one storage snapshot and select its head. A requested commit other
//!    than that head is rejected; historical reads are not implemented yet.
//! 2. `query` only: reject unsupported sort and offset options, then a
//!    `limit` of 0 or above `max_page_size`.
//! 3. Admit the filter, then evaluate it and materialize the page's records,
//!    at most `limit`, or with no limit `max_page_size`, from the same
//!    snapshot. Explicit budgets are not enforced yet.
//!
//! Report variants retain the selected snapshot's head even on failure,
//! including unavailable history and invalid requests. If snapshot/head
//! selection itself fails, there is no pricing commit to report.

use golemdb_branch::CommitId;
use golemdb_cells::CellReader;
use golemdb_index::Index;
use golemdb_merkle::HashProvider;
use golemdb_record::{Effects, Outcome, RecordError, committed_snapshot, read_committed_by_id};
use golemdb_storage::Store;

use crate::{
    CountOp, QueryError, QueryOp, QueryPage, Result, eval::match_set, plan::Plan,
    posting_source::IndexSource,
};

/// Query facade over the committed state of a store. Pending branch work is
/// never visible to a query. To see what a branch manager commits, pass a
/// clone of the store handle it writes, sharing its state, and its hasher.
#[derive(Clone)]
pub struct Queries<S, H> {
    store: S,
    hasher: H,
    max_query_depth: Option<u32>,
    max_query_limit: Option<u32>,
}

impl<S: Store, H: HashProvider> Queries<S, H> {
    /// `max_query_depth` caps how many `And` / `Or` levels a filter nests (a
    /// leaf adds none): a deeper one fails as [`QueryError::LimitExceeded`]
    /// at admission. Admission and evaluation recurse once per level, so with
    /// no cap a deep enough filter overflows the stack, aborting the process.
    ///
    /// `max_page_size` caps how many records a query returns, and so its
    /// `limit`: a larger limit fails as [`QueryError::LimitExceeded`]. A query
    /// with no limit that matches more still runs, and returns the first
    /// `max_page_size` matches in query order; the others are not read. With
    /// no cap, any limit holds, and a query with none returns every match.
    pub fn new(
        store: S,
        hasher: H,
        max_query_depth: Option<u32>,
        max_query_limit: Option<u32>,
    ) -> Self {
        Self {
            store,
            hasher,
            max_query_depth,
            max_query_limit,
        }
    }

    /// The records matching `op`'s filter at `commit` (`None` for the head),
    /// in record ID order, projected as `get` projects: the first `limit` of
    /// them, or with no limit the first `max_page_size`, while
    /// `total_matched` counts every match. A `limit` of 0 fails as
    /// [`QueryError::InvalidQuery`], one above `max_page_size` as
    /// [`QueryError::LimitExceeded`]. Sort and offset return `NotImplemented`
    /// for now, and the budget is not enforced yet. Use
    /// [`Self::query_report`] to retain the execution context on failure.
    pub fn query(&self, commit: Option<CommitId>, op: QueryOp) -> Result<QueryPage> {
        self.query_report(commit, op).result
    }

    /// Execute [`Self::query`] while retaining its pricing commit for receipts.
    pub fn query_report(
        &self,
        commit: Option<CommitId>,
        op: QueryOp,
    ) -> Outcome<QueryPage, QueryError> {
        self.read_report(commit, |snapshot, head| {
            if !op.sort.is_empty() {
                return Err(QueryError::NotImplemented {
                    operation: "query: sort",
                });
            }
            if op.offset.is_some() {
                return Err(QueryError::NotImplemented {
                    operation: "query: offset",
                });
            }
            let page_size = page_size(op.limit, self.max_query_limit)?;
            let plan = Plan::admit(&op.filter, self.max_query_depth)?;
            let index = Index::new(&self.hasher);
            let matches = match_set(&IndexSource::new(&index, snapshot), plan)?;
            let cells = CellReader::new(snapshot);
            let records = matches
                .treemap()
                .iter()
                .take(page_size)
                .map(|id| read_committed_by_id(&cells, id, op.projection.as_deref()))
                .collect::<golemdb_record::Result<_>>()?;
            Ok(QueryPage {
                records,
                total_matched: op.with_total.then(|| matches.treemap().len()),
                evaluated_at: head,
            })
        })
    }

    /// The size of the filter's match set at `commit` (`None` for the head),
    /// which `max_page_size` does not cap. The budget is not enforced yet.
    /// Use [`Self::count_report`] to retain the execution context on failure.
    pub fn count(&self, commit: Option<CommitId>, op: CountOp) -> Result<u64> {
        self.count_report(commit, op).result
    }

    /// Execute [`Self::count`] while retaining its pricing commit for receipts.
    pub fn count_report(&self, commit: Option<CommitId>, op: CountOp) -> Outcome<u64, QueryError> {
        self.read_report(commit, |snapshot, _| {
            let plan = Plan::admit(&op.filter, self.max_query_depth)?;
            let index = Index::new(&self.hasher);
            Ok(match_set(&IndexSource::new(&index, snapshot), plan)?
                .treemap()
                .len())
        })
    }

    fn read_report<T>(
        &self,
        commit: Option<CommitId>,
        operation: impl FnOnce(&S::Read<'_>, CommitId) -> Result<T>,
    ) -> Outcome<T, QueryError> {
        let (snapshot, head) = match committed_snapshot(&self.store, commit) {
            Ok(selected) => selected,
            Err(error) => {
                let priced_at = match &error {
                    RecordError::CommitUnavailable { head, .. } => Some(*head),
                    _ => None,
                };
                return Outcome::new(Err(error.into()), priced_at, Effects::default());
            }
        };
        Outcome::new(operation(&snapshot, head), Some(head), Effects::default())
    }
}

/// How many records a page holds at most: the query's `limit`, which must
/// be at least 1 and at most `max_page_size`; with no limit, `max_page_size`;
/// with neither, every match.
fn page_size(limit: Option<u64>, max_page_size: Option<u32>) -> Result<usize> {
    match (limit, max_page_size) {
        (Some(0), _) => Err(QueryError::InvalidQuery {
            reason: "`limit` must be at least 1",
        }),
        (Some(limit), Some(max)) if limit > u64::from(max) => Err(QueryError::LimitExceeded {
            limit: "query limit",
            max,
        }),
        // A limit no `usize` holds takes every match, as no limit does.
        (Some(limit), _) => Ok(usize::try_from(limit).unwrap_or(usize::MAX)),
        (None, Some(max)) => Ok(max as usize),
        (None, None) => Ok(usize::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_limit_a_page_holds_up_to_the_page_size() {
        assert_eq!(page_size(None, Some(100)).unwrap(), 100);
    }

    #[test]
    fn with_no_limit_and_no_page_size_a_page_holds_every_match() {
        assert_eq!(page_size(None, None).unwrap(), usize::MAX);
    }

    #[test]
    fn a_limit_up_to_the_page_size_sizes_the_page() {
        for limit in [1, 99, 100] {
            assert_eq!(page_size(Some(limit), Some(100)).unwrap(), limit as usize);
        }
    }

    #[test]
    fn a_limit_above_the_page_size_exceeds_it() {
        for limit in [101, u64::from(u32::MAX) + 1, u64::MAX] {
            assert!(
                matches!(
                    page_size(Some(limit), Some(100)),
                    Err(QueryError::LimitExceeded {
                        limit: "query limit",
                        max: 100
                    })
                ),
                "{limit}"
            );
        }
    }

    #[test]
    fn with_no_page_size_any_limit_holds() {
        assert_eq!(page_size(Some(1_000_000), None).unwrap(), 1_000_000);
        assert_eq!(page_size(Some(u64::MAX), None).unwrap(), usize::MAX);
    }

    #[test]
    fn a_limit_of_zero_is_an_invalid_query() {
        for max_page_size in [None, Some(0), Some(100)] {
            assert!(
                matches!(
                    page_size(Some(0), max_page_size),
                    Err(QueryError::InvalidQuery { .. })
                ),
                "{max_page_size:?}"
            );
        }
    }
}
