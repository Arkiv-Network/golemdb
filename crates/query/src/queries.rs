//! [`Queries`]: `query` and `count` over committed state.
//!
//! Checks run in the proposal's order: the target first, at no cost; then
//! admission, validating the request before any state read; then the reads,
//! all in one storage snapshot.

use golemdb_branch::{Branches, CommitId, read_head};
use golemdb_cells::{CellKey, CellName, CellReader, reserved};
use golemdb_index::{Bitmap, Index};
use golemdb_merkle::HashProvider;
use golemdb_record::{ReadTarget, Record, RecordCells, RecordKey};
use golemdb_storage::{ReadTransaction, Store};

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
        let target = Committed::try_from(target)?;
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
            .map(|id| record(&cells, id, projection.as_deref()))
            .collect::<Result<_>>()?;
        Ok(QueryPage {
            records,
            total_matched: with_total.then(|| matches.treemap().len()),
            evaluated_at: head,
        })
    }

    fn run_count(&self, target: ReadTarget, op: CountOp) -> Result<u64> {
        let target = Committed::try_from(target)?;
        let CountOp { filter, budget: _ } = op;
        Ok(self.evaluate(target, &filter)?.matches.treemap().len())
    }

    /// ToDo: I don't like this one, it works but there's something that I am missing...
    /// I'll refactor it shortly.
    /// 
    /// Shared by `query` and `count`: admit `filter`, then evaluate it in one
    /// snapshot at the target commit. The snapshot is returned so the caller
    /// can keep reading the same state.
    fn evaluate(&self, target: Committed, filter: &Filter) -> Result<Evaluation<S::Read<'_>>> {
        let plan = Plan::try_from(filter)?;
        let snapshot = self.branches.store().begin_read()?;
        let head = read_head(&snapshot)?;
        if let Committed::Commit(requested) = target
            && requested != head
        {
            return Err(QueryError::CommitUnavailable { requested, head });
        }
        let index = Index::new(self.branches.hasher());
        let matches = match_set(&IndexSource::new(&index, &snapshot), &plan)?;
        Ok(Evaluation {
            snapshot,
            head,
            matches,
        })
    }
}

/// A target a query can read: committed state only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Committed {
    Head,
    Commit(CommitId),
}

impl TryFrom<ReadTarget> for Committed {
    type Error = QueryError;

    fn try_from(target: ReadTarget) -> Result<Self> {
        match target {
            ReadTarget::Head => Ok(Self::Head),
            ReadTarget::Commit(commit) => Ok(Self::Commit(commit)),
            ReadTarget::Branch(_) => Err(QueryError::InvalidQuery {
                reason: "queries read committed state only, not a branch",
            }),
        }
    }
}

/// A filter's match set, with the snapshot and head it was evaluated at.
struct Evaluation<T> {
    snapshot: T,
    head: CommitId,
    matches: Bitmap,
}

/// The record `id` as `get` returns it: every cell including `#key`, or only
/// the projected cells, with missing ones omitted.
fn record<R: ReadTransaction>(
    cells: &CellReader<'_, R>,
    id: u64,
    projection: Option<&[CellName]>,
) -> Result<Record> {
    let key = cells
        .get(&CellKey::new(id, reserved::KEY))?
        .and_then(|value| value.as_bytes32())
        .map(RecordKey)
        .ok_or_else(|| QueryError::corruption("indexed record has no #key"))?;
    let mut values = RecordCells::new();
    match projection {
        Some(names) => {
            for name in names {
                if let Some(value) = cells.get(&CellKey::new(id, name.clone()))? {
                    values.insert(name.clone(), value);
                }
            }
        }
        None => {
            for row in cells.scan_record(id)? {
                let (address, value) = row?;
                values.insert(address.name().into(), value);
            }
        }
    }
    Ok(Record { key, cells: values })
}

#[cfg(test)]
mod tests {
    use golemdb_branch::BranchId;

    use super::*;

    #[test]
    fn committed_targets_are_head_and_commit() {
        let commit = CommitId::new(7);
        assert_eq!(
            Committed::try_from(ReadTarget::Head).expect("committed"),
            Committed::Head
        );
        assert_eq!(
            Committed::try_from(ReadTarget::Commit(commit)).expect("committed"),
            Committed::Commit(commit)
        );
    }

    #[test]
    fn branch_targets_are_invalid_queries() {
        assert!(matches!(
            Committed::try_from(ReadTarget::Branch(BranchId::new(1))),
            Err(QueryError::InvalidQuery { .. })
        ));
    }
}
