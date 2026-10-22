use crate::{ApiError, CommitId, Result};

/// Every record/query outcome includes a receipt. Explicitly discard it with
/// `into_result()` only when another layer does not need to account for the call.
#[derive(Debug)]
#[must_use]
pub struct Metered<T> {
    pub result: Result<T>,
    pub receipt: Receipt,
}

impl<T> Metered<T> {
    pub fn new(result: Result<T>, receipt: Receipt) -> Self {
        Self { result, receipt }
    }
    /// Zero-cost receipt for mocks and unmetered implementations.
    pub fn unmetered(result: Result<T>, priced_at: Option<CommitId>) -> Self {
        Self::new(
            result,
            Receipt {
                cost: 0,
                priced_at,
                details: Details::default(),
            },
        )
    }
    pub fn into_result(self) -> Result<T> {
        self.result
    }
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Metered<U> {
        Metered {
            result: self.result.map(f),
            receipt: self.receipt,
        }
    }
    pub fn map_err(self, f: impl FnOnce(ApiError) -> ApiError) -> Self {
        Self {
            result: self.result.map_err(f),
            receipt: self.receipt,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Receipt {
    /// Zero until operation pricing is implemented.
    pub cost: u64,
    pub priced_at: Option<CommitId>,
    pub details: Details,
}

/// Actual user-cell effects; system cells, failed writes and reads count zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Details {
    pub cells_created: u64,
    pub cells_updated: u64,
    pub cells_deleted: u64,
    pub index_joins: u64,
    pub index_leaves: u64,
    pub cell_bytes_written: u64,
    pub cell_bytes_deleted: u64,
    pub index_bytes_written: u64,
    pub index_bytes_deleted: u64,
}

impl From<golemdb_record::Effects> for Details {
    fn from(e: golemdb_record::Effects) -> Self {
        Self {
            cells_created: e.cells_created,
            cells_updated: e.cells_updated,
            cells_deleted: e.cells_deleted,
            index_joins: e.index_joins,
            index_leaves: e.index_leaves,
            cell_bytes_written: e.cell_bytes_written,
            cell_bytes_deleted: e.cell_bytes_deleted,
            index_bytes_written: e.index_bytes_written,
            index_bytes_deleted: e.index_bytes_deleted,
        }
    }
}

impl<T, E: Into<ApiError>> From<golemdb_record::Outcome<T, E>> for Metered<T> {
    fn from(outcome: golemdb_record::Outcome<T, E>) -> Self {
        Self {
            result: outcome.result.map_err(Into::into),
            receipt: Receipt {
                cost: 0,
                priced_at: outcome.priced_at,
                details: outcome.effects.into(),
            },
        }
    }
}
