//! [`Queries`]: `query` and `count` over committed state.
//!
//! Checks run in this order:
//!
//! 1. `query` only: the parts not built yet (sort, limit, offset) are
//!    rejected as `NotImplemented`.
//! 2. Admission validates the whole request, the filter's depth included,
//!    without reading state.
//! 3. One storage snapshot is opened. Opening it reads the head, so a
//!    requested commit other than the head fails here, after admission. All
//!    reads then use this snapshot.

use golemdb_branch::CommitId;
use golemdb_cells::CellReader;
use golemdb_index::{Bitmap, Index};
use golemdb_merkle::HashProvider;
use golemdb_record::{RecordKey, committed_bindings, committed_snapshot, read_committed_by_id};
use golemdb_storage::Store;

use crate::{
    CountOp, Filter, QueryError, QueryOp, QueryPage, Result, eval::match_set, plan::Plan,
    posting_source::IndexSource,
};

/// Up to this many matches, `query_keys` reads each record's `#key`; past it,
/// one scan of every binding is cheaper than that many scattered reads.
const KEYS_READ_ONE_BY_ONE: u64 = 1 << 16;

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
    pub fn query(&self, commit_id: Option<CommitId>, op: QueryOp) -> Result<QueryPage> {
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
        } = self.evaluate(commit_id, &filter)?;
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

    /// The keys of the records matching `op`'s filter at `commit` (`None` for
    /// the head), ascending, with no record materialized: a large match set
    /// is read off the key bindings, which are stored in key order, and a
    /// small one from each record's `#key`, then sorted. The budget is not
    /// enforced yet.
    pub fn query_keys(&self, commit_id: Option<CommitId>, op: CountOp) -> Result<Vec<RecordKey>> {
        let CountOp { filter, budget: _ } = op;
        let Evaluation {
            snapshot, matches, ..
        } = self.evaluate(commit_id, &filter)?;
        let cells = CellReader::new(&snapshot);
        let ids = matches.treemap();
        let mut keys = Vec::with_capacity(ids.len() as usize);
        if ids.len() <= KEYS_READ_ONE_BY_ONE {
            for id in ids.iter() {
                keys.push(read_committed_by_id(&cells, id, Some(&[]))?.key);
            }
            keys.sort_unstable();
        } else {
            for binding in committed_bindings(&cells)? {
                let (key, id) = binding?;
                if ids.contains(id) {
                    keys.push(key);
                }
            }
        }
        Ok(keys)
    }

    /// The size of the filter's match set at `commit` (`None` for the head).
    /// The budget is not enforced yet.
    pub fn count(&self, commit_id: Option<CommitId>, op: CountOp) -> Result<u64> {
        let CountOp { filter, budget: _ } = op;
        Ok(self.evaluate(commit_id, &filter)?.matches.treemap().len())
    }

    /// Shared by `query` and `count`: admit `filter`, then open one snapshot,
    /// check the requested commit against its head, and evaluate `filter`
    /// there. The snapshot is returned so the caller can keep reading the
    /// same state.
    fn evaluate(
        &self,
        commit_id: Option<CommitId>,
        filter: &Filter,
    ) -> Result<Evaluation<S::Read<'_>>> {
        let plan = Plan::admit(filter, self.max_query_depth)?;
        let (snapshot, head) = committed_snapshot(&self.store, commit_id)?;
        let index = Index::new(&self.hasher);
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
