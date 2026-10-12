//! [`Queries`]: `query` and `count` over committed state.
//!
//! Checks run in the proposal's order: the target first, at no cost; then
//! admission, validating the request before any state read; then the reads,
//! all in one storage snapshot.

use golemdb_branch::{Branches, CommitId};
use golemdb_cells::CellReader;
use golemdb_index::{Bitmap, Index};
use golemdb_merkle::HashProvider;
use golemdb_record::{ReadTarget, committed_snapshot, read_committed_by_id};
use golemdb_storage::Store;

use crate::{
    CountOp, Filter, Metered, QueryError, QueryOp, QueryPage, Result, eval::match_set, plan::Plan,
    source::IndexSource,
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

    /// The records matching `op`'s filter, in record ID order, projected as
    /// `get` projects. Sort, limit and offset return `NotImplemented` for now,
    /// and the budget is not enforced yet.
    pub fn query(&self, target: ReadTarget, op: QueryOp) -> Metered<QueryPage> {
        Metered {
            result: self.run_query(target, op),
        }
    }

    /// The size of the filter's match set. The budget is not enforced yet.
    pub fn count(&self, target: ReadTarget, op: CountOp) -> Metered<u64> {
        Metered {
            result: self.run_count(target, op),
        }
    }

    fn run_query(&self, target: ReadTarget, op: QueryOp) -> Result<QueryPage> {
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
        } = self.evaluate(target, &filter)?;
        let cells = CellReader::new(&snapshot);
        let records = matches
            .treemap()
            .iter()
            .map(|id| {
                read_committed_by_id(&cells, id, projection.as_deref())?
                    .ok_or_else(|| QueryError::corruption("indexed record has no #key"))
            })
            .collect::<Result<_>>()?;
        Ok(QueryPage {
            records,
            total_matched: with_total.then(|| matches.treemap().len()),
            evaluated_at: head,
        })
    }

    fn run_count(&self, target: ReadTarget, op: CountOp) -> Result<u64> {
        let CountOp { filter, budget: _ } = op;
        Ok(self.evaluate(target, &filter)?.matches.treemap().len())
    }

    /// Shared by `query` and `count`: reject a branch target, admit `filter`,
    /// then evaluate it in one snapshot at the target commit. The snapshot is
    /// returned so the caller can keep reading the same state.
    fn evaluate(&self, target: ReadTarget, filter: &Filter) -> Result<Evaluation<S::Read<'_>>> {
        let requested = match target {
            ReadTarget::Head => None,
            ReadTarget::Commit(commit) => Some(commit),
            ReadTarget::Branch(_) => {
                return Err(QueryError::InvalidQuery {
                    reason: "queries read committed state only, not a branch",
                });
            }
        };
        let plan = Plan::try_from(filter)?;
        let (snapshot, head) = committed_snapshot(self.branches.store(), requested)?;
        let index = Index::new(self.branches.hasher());
        let matches = match_set(&IndexSource::new(&index, &snapshot), &plan)?;
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
