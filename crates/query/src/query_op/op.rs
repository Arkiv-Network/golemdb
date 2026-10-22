//! [`QueryOp`] and [`CountOp`]: typed requests built step by step.
//!
//! Budgets are explicit at construction. Steps never fail; an invalid name, a cap exceeded, or
//! both `after` and `offset` are reported by the call that runs the request.

use golemdb_cells::CellName;

use crate::{Budget, Filter, SortKey};

/// A filtered, optionally sorted and projected, paged read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryOp {
    pub filter: Filter,
    /// Empty: record ID ascending.
    pub sort: Vec<SortKey>,
    /// `None` selects full records; `Some([])` selects no cells.
    pub projection: Option<Vec<CellName>>,
    /// The most records the page holds, the first in query order. `None`:
    /// up to the page size of the [`Queries`](crate::Queries) it runs on.
    pub limit: Option<u64>,
    // /// Resume after a previous page. At most one of `after` and `offset`.
    // pub after: Option<Cursor>,
    pub offset: Option<u64>,
    pub with_total: bool,
    /// Explicit budget, stored but not enforced yet.
    pub budget: Budget,
}

impl QueryOp {
    pub fn from_filter(filter: Filter, budget: Budget) -> Self {
        Self {
            filter,
            sort: Vec::new(),
            projection: None,
            limit: None,
            // after: None,
            offset: None,
            with_total: false,
            budget,
        }
    }

    /// Replace the ordering with `keys`, most significant first.
    pub fn sort(mut self, keys: impl IntoIterator<Item = SortKey>) -> Self {
        self.sort = keys.into_iter().collect();
        self
    }

    /// Replace the projection, as for `get`: raw names, deduplicated in byte
    /// order. An empty selection returns no cells.
    pub fn only<I, N>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = N>,
        N: AsRef<[u8]>,
    {
        self.projection = Some(CellName::projection(names));
        self
    }

    /// Return at most `limit` records, the first in query order. The call
    /// rejects a limit of 0, and one above the page size of the
    /// [`Queries`](crate::Queries) it runs on.
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Ask for `total_matched`.
    pub fn with_total(mut self) -> Self {
        self.with_total = true;
        self
    }

    pub fn budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }
}

/// The size of a filter's match set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountOp {
    pub filter: Filter,
    /// Explicit budget, stored but not enforced yet.
    pub budget: Budget,
}

impl CountOp {
    pub fn filter(filter: Filter, budget: Budget) -> Self {
        Self { filter, budget }
    }

    pub fn budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{CellNameRef, CellType, Width, marker::Str};

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
                filter: color_is_blue(),
                sort: vec![],
                projection: None,
                limit: None,
                offset: None,
                with_total: false,
                budget: Budget::Limited(u64::MAX),
            }
        );
    }

    #[test]
    fn every_step_sets_its_field() {
        let price = SortKey::asc("price", CellType::Uint(Width::W8));
        let op = QueryOp::from_filter(color_is_blue(), crate::Budget::Limited(u64::MAX))
            .sort([price.clone()])
            .only(["owner", "status"])
            .limit(50)
            .offset(10)
            .with_total()
            .budget(Budget::Limited(1_000));
        assert_eq!(
            op,
            QueryOp {
                filter: color_is_blue(),
                sort: vec![price],
                projection: Some(vec![name(b"owner"), name(b"status")]),
                limit: Some(50),
                offset: Some(10),
                with_total: true,
                budget: Budget::Limited(1_000),
            }
        );
    }

    #[test]
    fn projection_is_deduplicated_in_byte_order_and_may_be_empty() {
        let op = QueryOp::from_filter(color_is_blue(), crate::Budget::Limited(u64::MAX))
            .only(["status", "owner", "status"]);
        assert_eq!(op.projection, Some(vec![name(b"owner"), name(b"status")]));
        let none = QueryOp::from_filter(color_is_blue(), crate::Budget::Limited(u64::MAX))
            .only(Vec::<&str>::new());
        assert_eq!(none.projection, Some(vec![]));
    }

    #[test]
    fn repeated_steps_replace_earlier_ones() {
        let price = SortKey::asc("price", CellType::Uint(Width::W8));
        let date = SortKey::desc("date", CellType::Date32);
        let op = QueryOp::from_filter(color_is_blue(), crate::Budget::Limited(u64::MAX))
            .sort([price])
            .sort([date.clone()])
            .limit(1)
            .limit(2)
            .budget(Budget::Limited(1))
            .budget(Budget::Unlimited);
        assert_eq!(op.sort, [date]);
        assert_eq!(op.limit, Some(2));
        assert_eq!(op.budget, Budget::Unlimited);
    }

    #[test]
    fn a_count_takes_a_filter_and_a_budget() {
        assert_eq!(
            CountOp::filter(color_is_blue(), crate::Budget::Limited(u64::MAX))
                .budget(Budget::Limited(7)),
            CountOp {
                filter: color_is_blue(),
                budget: Budget::Limited(7),
            }
        );
        assert_eq!(
            CountOp::filter(color_is_blue(), crate::Budget::Limited(u64::MAX)).budget,
            Budget::Limited(u64::MAX)
        );
    }
}
