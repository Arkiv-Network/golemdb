//! [`Queries`]: `query` and `count` over committed state.
//!
//! Checks run in this order:
//!
//! 1. Open one storage snapshot and select its head. A requested commit other
//!    than that head is rejected; historical reads are not implemented yet.
//! 2. `query` only: reject unsupported sort, limit and offset options.
//! 3. Admit the filter, then evaluate it and materialize records from the same
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
}

impl<S: Store, H: HashProvider> Queries<S, H> {
    /// `max_query_depth` caps how many `And` / `Or` levels a filter nests (a
    /// leaf adds none): a deeper one fails as [`QueryError::LimitExceeded`]
    /// at admission. Admission and evaluation recurse once per level, so with
    /// no cap a deep enough filter overflows the stack, aborting the process.
    pub fn new(store: S, hasher: H, max_query_depth: Option<u32>) -> Self {
        Self {
            store,
            hasher,
            max_query_depth,
        }
    }

    /// The records matching `op`'s filter at `commit` (`None` for the head),
    /// in record ID order, projected as `get` projects. Sort, limit and offset
    /// return `NotImplemented` for now, and the budget is not enforced yet.
    /// Use [`Self::query_report`] to retain the execution context on failure.
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
            if op.limit.is_some() {
                return Err(QueryError::NotImplemented {
                    operation: "query: limit",
                });
            }
            if op.offset.is_some() {
                return Err(QueryError::NotImplemented {
                    operation: "query: offset",
                });
            }
            let plan = Plan::admit(&op.filter, self.max_query_depth)?;
            let index = Index::new(&self.hasher);
            let matches = match_set(&IndexSource::new(&index, snapshot), plan)?;
            let cells = CellReader::new(snapshot);
            let records = matches
                .treemap()
                .iter()
                .map(|id| read_committed_by_id(&cells, id, op.projection.as_deref()))
                .collect::<golemdb_record::Result<_>>()?;
            Ok(QueryPage {
                records,
                total_matched: op.with_total.then(|| matches.treemap().len()),
                evaluated_at: head,
            })
        })
    }

    /// The size of the filter's match set at `commit` (`None` for the head).
    /// The budget is not enforced yet. Use [`Self::count_report`] to retain
    /// the execution context on failure.
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
