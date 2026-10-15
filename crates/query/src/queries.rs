//! [`Queries`]: `query` and `count` over committed state.
//!
//! Checks run in this order:
//!
//! 1. `query` only: the parts not built yet (sort, limit, offset) are
//!    rejected as `NotImplemented`.
//! 2. Admission validates the whole request, without reading state.
//! 3. One storage snapshot is opened. Opening it reads the head, so a
//!    requested commit other than the head fails here, after admission. All
//!    reads then use this snapshot.

use golemdb_branch::{Branches, CommitId};
use golemdb_cells::CellReader;
use golemdb_index::{Bitmap, Index};
use golemdb_merkle::HashProvider;
use golemdb_record::{committed_snapshot, read_committed_by_id};
use golemdb_storage::Store;

use crate::{
    CountOp, Filter, QueryError, QueryOp, QueryPage, Result, eval::match_set, plan::Plan,
    posting_source::IndexSource,
};

/// Query facade over the committed state behind a branch manager. Clones
/// share it. Pending branch work is never visible to a query.
pub struct Queries<S, H> {
    branches: Branches<S, H>,
}

impl<S, H> Clone for Queries<S, H> {
    fn clone(&self) -> Self {
        Self {
            branches: self.branches.clone(),
        }
    }
}

impl<S: Store, H: HashProvider> Queries<S, H> {
    pub fn new(branches: Branches<S, H>) -> Self {
        Self { branches }
    }

    /// The records matching `op`'s filter at `commit` (`None` for the head),
    /// in record ID order, projected as `get` projects. Sort, limit and offset
    /// return `NotImplemented` for now, and the budget is not enforced yet.
    pub fn query(&self, commit: Option<CommitId>, op: QueryOp) -> Result<QueryPage> {
        self.run_query(commit, op)
    }

    /// The size of the filter's match set at `commit` (`None` for the head).
    /// The budget is not enforced yet.
    pub fn count(&self, commit: Option<CommitId>, op: CountOp) -> Result<u64> {
        self.run_count(commit, op)
    }

    fn run_query(&self, commit: Option<CommitId>, op: QueryOp) -> Result<QueryPage> {
        let QueryOp {
            filter,
            sort,
            projection,
            limit,
            offset,
            with_total,
            budget: _,
        } = op;
        if !sort.is_empty() {
            return Err(QueryError::NotImplemented {
                operation: "query: sort",
            });
        }
        if limit.is_some() {
            return Err(QueryError::NotImplemented {
                operation: "query: limit",
            });
        }
        if offset.is_some() {
            return Err(QueryError::NotImplemented {
                operation: "query: offset",
            });
        }
        let Evaluation {
            snapshot,
            head,
            matches,
        } = self.evaluate(commit, &filter)?;
        let cells = CellReader::new(&snapshot);
        let records = matches
            .treemap()
            .iter()
            .map(|id| read_committed_by_id(&cells, id, projection.as_deref()))
            .collect::<golemdb_record::Result<_>>()?;
        Ok(QueryPage {
            records,
            total_matched: with_total.then(|| matches.treemap().len()),
            evaluated_at: head,
        })
    }

    fn run_count(&self, commit: Option<CommitId>, op: CountOp) -> Result<u64> {
        let CountOp { filter, budget: _ } = op;
        Ok(self.evaluate(commit, &filter)?.matches.treemap().len())
    }

    /// Shared by `query` and `count`: admit `filter`, then open one snapshot,
    /// check the requested commit against its head, and evaluate `filter`
    /// there. The snapshot is returned so the caller can keep reading the
    /// same state.
    fn evaluate(
        &self,
        commit: Option<CommitId>,
        filter: &Filter,
    ) -> Result<Evaluation<S::Read<'_>>> {
        let plan = Plan::try_from(filter)?;
        let (snapshot, head) = committed_snapshot(self.branches.store(), commit)?;
        let index = Index::new(self.branches.hasher());
        let matches = match_set(&IndexSource::new(&index, &snapshot), plan)?;
        Ok(Evaluation {
            snapshot,
            head,
            matches,
        })
    }
}

/// A filter's match set, with the snapshot and head it was evaluated at.
struct Evaluation<T> {
    snapshot: T,
    head: CommitId,
    matches: Bitmap,
}
