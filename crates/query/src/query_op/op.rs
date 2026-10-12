//! [`QueryOp`] and [`CountOp`]: typed requests built step by step.
//!
//! Steps never fail. A missing budget, an invalid name, a cap exceeded, or
//! both `after` and `offset` are reported by the call that runs the request.

use std::collections::BTreeSet;

use golemdb_cells::{CellName, CellNameRef};

use crate::{Budget, Filter, SortKey};

/// A filtered, optionally sorted and projected, paged read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryOp {
    pub filter: Filter,
    /// Empty: record ID ascending.
    pub sort: Vec<SortKey>,
    /// `None` selects full records; `Some([])` selects no cells.
    pub projection: Option<Vec<CellName>>,
    pub limit: Option<u64>,
    // /// Resume after a previous page. At most one of `after` and `offset`.
    // pub after: Option<Cursor>,
    pub offset: Option<u64>,
    pub with_total: bool,
    /// Required; `None` fails at the call.
    pub budget: Option<Budget>,
}

impl QueryOp {
    pub fn filter(filter: Filter) -> Self {
        Self {
            filter,
            sort: Vec::new(),
            projection: None,
            limit: None,
            // after: None,
            offset: None,
            with_total: false,
            budget: None,
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
        self.projection = Some(
            names
                .into_iter()
                .map(|name| CellName::from(CellNameRef::raw(name.as_ref())))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        );
        self
    }

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
        self.budget = Some(budget);
        self
    }
}

/// The size of a filter's match set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountOp {
    pub filter: Filter,
    /// Required; `None` fails at the call.
    pub budget: Option<Budget>,
}

impl CountOp {
    pub fn filter(filter: Filter) -> Self {
        Self {
            filter,
            budget: None,
        }
    }

    pub fn budget(mut self, budget: Budget) -> Self {
        self.budget = Some(budget);
        self
    }
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{CellType, CellValue, Width};

    use super::*;

    fn name(bytes: &[u8]) -> CellName {
        CellNameRef::raw(bytes).into()
    }

    fn color_is_blue() -> Filter {
        Filter::Eq {
            field: name(b"color"),
            value: CellValue::from_str("blue"),
        }
    }

    #[test]
    fn a_query_starts_with_only_its_filter() {
        assert_eq!(
            QueryOp::filter(color_is_blue()),
            QueryOp {
                filter: color_is_blue(),
                sort: vec![],
                projection: None,
                limit: None,
                offset: None,
                with_total: false,
                budget: None,
            }
        );
    }

    #[test]
    fn every_step_sets_its_field() {
        let price = SortKey::asc("price", CellType::Uint(Width::W8));
        let op = QueryOp::filter(color_is_blue())
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
                budget: Some(Budget::Limited(1_000)),
            }
        );
    }

    #[test]
    fn projection_is_deduplicated_in_byte_order_and_may_be_empty() {
        let op = QueryOp::filter(color_is_blue()).only(["status", "owner", "status"]);
        assert_eq!(op.projection, Some(vec![name(b"owner"), name(b"status")]));
        let none = QueryOp::filter(color_is_blue()).only(Vec::<&str>::new());
        assert_eq!(none.projection, Some(vec![]));
    }

    #[test]
    fn repeated_steps_replace_earlier_ones() {
        let price = SortKey::asc("price", CellType::Uint(Width::W8));
        let date = SortKey::desc("date", CellType::Date32);
        let op = QueryOp::filter(color_is_blue())
            .sort([price])
            .sort([date.clone()])
            .limit(1)
            .limit(2)
            .budget(Budget::Limited(1))
            .budget(Budget::Unlimited);
        assert_eq!(op.sort, [date]);
        assert_eq!(op.limit, Some(2));
        assert_eq!(op.budget, Some(Budget::Unlimited));
    }

    #[test]
    fn a_count_takes_a_filter_and_a_budget() {
        assert_eq!(
            CountOp::filter(color_is_blue()).budget(Budget::Limited(7)),
            CountOp {
                filter: color_is_blue(),
                budget: Some(Budget::Limited(7)),
            }
        );
        assert_eq!(CountOp::filter(color_is_blue()).budget, None);
    }
}
