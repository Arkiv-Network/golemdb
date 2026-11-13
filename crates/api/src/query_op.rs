//! Query and count operation builders. Execution belongs to `golemdb_query`.
//!
//! Budgets are explicit at construction and passed separately to execution.
//! Builder steps do not validate requests; admission runs after snapshot selection.
//! Query projections retain their existing deduplication in byte order.

use golemdb_query::QueryRequest;

use crate::{Budget, CellName, Filter, SortKey};

/// A filtered, optionally sorted and projected, paged read with an explicit budget.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct QueryOp {
    request: QueryRequest,
    budget: Budget,
}

impl QueryOp {
    pub fn from_filter(filter: Filter, budget: Budget) -> Self {
        Self {
            request: QueryRequest {
                filter,
                sort: Vec::new(),
                projection: None,
                limit: None,
                offset: None,
                with_total: false,
            },
            budget,
        }
    }

    /// Replace the ordering with `keys`, most significant first.
    pub fn sort(mut self, keys: impl IntoIterator<Item = SortKey>) -> Self {
        self.request.sort = keys.into_iter().collect();
        self
    }

    /// Replace the projection: raw names, deduplicated in byte order.
    /// An empty selection returns no cells.
    pub fn only<I, N>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = N>,
        N: AsRef<[u8]>,
    {
        self.request.projection = Some(CellName::projection(names));
        self
    }

    /// Return at most `limit` records, the first in query order. The call
    /// rejects a limit of 0, and one above the `max_query_limit` of the
    /// [`golemdb_query::Queries`] it runs on.
    pub fn limit(mut self, limit: u64) -> Self {
        self.request.limit = Some(limit);
        self
    }

    /// Start the page past the first `offset` matches, in query order. The
    /// next page's offset is this one plus the records returned; an offset
    /// past the last match gives an empty page.
    pub fn offset(mut self, offset: u64) -> Self {
        self.request.offset = Some(offset);
        self
    }

    /// Ask for `total_matched`.
    pub fn with_total(mut self) -> Self {
        self.request.with_total = true;
        self
    }

    pub fn budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// The query description, for inspection without consuming the operation.
    pub fn request(&self) -> &QueryRequest {
        &self.request
    }

    /// The explicit budget passed to query execution.
    pub fn requested_budget(&self) -> Budget {
        self.budget
    }

    pub(crate) fn into_parts(self) -> (QueryRequest, Budget) {
        (self.request, self.budget)
    }
}

/// The size of a filter's match set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct CountOp {
    filter: Filter,
    /// Explicit budget, stored but not enforced yet.
    budget: Budget,
}

impl CountOp {
    pub fn filter(filter: Filter, budget: Budget) -> Self {
        Self { filter, budget }
    }

    pub fn budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// The filter, for inspection without consuming the operation.
    pub fn predicate(&self) -> &Filter {
        &self.filter
    }

    /// The explicit budget passed to count execution.
    pub fn requested_budget(&self) -> Budget {
        self.budget
    }

    pub(crate) fn into_parts(self) -> (Filter, Budget) {
        (self.filter, self.budget)
    }
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{
        CellNameRef,
        marker::{Date32, Str, U64},
    };

    use super::*;
    use crate::EqValue;

    fn name(bytes: &[u8]) -> CellName {
        CellNameRef::raw(bytes).into()
    }

    fn color_is_blue() -> Filter {
        Filter::Eq {
            field: name(b"color"),
            value: EqValue::of(Str, "blue"),
        }
    }

    #[test]
    fn a_query_starts_with_only_its_filter() {
        assert_eq!(
            QueryOp::from_filter(color_is_blue(), crate::Budget::Limited(u64::MAX)),
            QueryOp {
                request: QueryRequest {
                    filter: color_is_blue(),
                    sort: vec![],
                    projection: None,
                    limit: None,
                    offset: None,
                    with_total: false,
                },
                budget: Budget::Limited(u64::MAX),
            }
        );
    }

    #[test]
    fn every_step_sets_its_field() {
        let price = SortKey::asc("price", U64);
        let op = QueryOp::from_filter(color_is_blue(), crate::Budget::Limited(u64::MAX))
            .sort([price.clone()])
            .only(["owner", "status"])
            .limit(50)
            .offset(10)
            .with_total()
            .budget(Budget::Limited(1_000));
        assert_eq!(
            op.into_parts(),
            (
                QueryRequest {
                    filter: color_is_blue(),
                    sort: vec![price],
                    projection: Some(vec![name(b"owner"), name(b"status")]),
                    limit: Some(50),
                    offset: Some(10),
                    with_total: true,
                },
                Budget::Limited(1_000),
            )
        );
    }

    #[test]
    fn projection_is_deduplicated_in_byte_order_and_may_be_empty() {
        let op = QueryOp::from_filter(color_is_blue(), crate::Budget::Limited(u64::MAX))
            .only(["status", "owner", "status"]);
        assert_eq!(
            op.request().projection,
            Some(vec![name(b"owner"), name(b"status")])
        );
        let none = QueryOp::from_filter(color_is_blue(), crate::Budget::Limited(u64::MAX))
            .only(Vec::<&str>::new());
        assert_eq!(none.request().projection, Some(vec![]));
    }

    #[test]
    fn repeated_steps_replace_earlier_ones() {
        let price = SortKey::asc("price", U64);
        let date = SortKey::desc("date", Date32);
        let op = QueryOp::from_filter(color_is_blue(), crate::Budget::Limited(u64::MAX))
            .sort([price])
            .sort([date.clone()])
            .limit(1)
            .limit(2)
            .budget(Budget::Limited(1))
            .budget(Budget::Unlimited);
        assert_eq!(op.request().sort, [date]);
        assert_eq!(op.request().limit, Some(2));
        assert_eq!(op.requested_budget(), Budget::Unlimited);
    }

    #[test]
    fn a_count_takes_a_filter_and_a_budget() {
        let op = CountOp::filter(color_is_blue(), Budget::Limited(u64::MAX));
        assert_eq!(op.predicate(), &color_is_blue());
        assert_eq!(op.requested_budget(), Budget::Limited(u64::MAX));
        assert_eq!(
            op.budget(Budget::Limited(7)).into_parts(),
            (color_is_blue(), Budget::Limited(7))
        );
    }
}
