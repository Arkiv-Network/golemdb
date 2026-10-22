//! [`Plan`]: a filter after admission. Building it validates the whole filter,
//! its depth included, before index or record reads and resolves its leaves to the
//! index work they need. Pure; no storage access.

use std::collections::BTreeSet;

use golemdb_cells::{CellName, CellValue};
use golemdb_index::{IndexTerm, RangeError, TermError, TermRange};

use crate::{Bounds, Filter, InValues, QueryError, Result};

/// The written filter with its terms resolved: same shape, same order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// One term read: the postings of one exact term.
    TermRead(IndexTerm),
    /// One term read per term, in the set's order (ascending term key, as
    /// the proposal requires; `IndexTerm` orders by its bytes), each term
    /// once: the union of their postings.
    TermReads(BTreeSet<IndexTerm>),
    /// A scan of the range's terms in ascending key order, one term read
    /// each: the union of their postings. `Range` and `Prefix` leaves.
    Scan(TermRange),
    /// A leaf no record can match, an `In` with no value or a `Range` whose
    /// bounds admit none: nothing is read.
    Empty,
    /// The intersection of the children's match sets, evaluated in the
    /// written order, `first` then `rest`, until the running set is empty.
    And { first: Box<Plan>, rest: Vec<Plan> },
    /// The union of every child's match set, in the written order.
    Or { first: Box<Plan>, rest: Vec<Plan> },
}

impl Plan {
    /// Admission, depth-first and children left to right, so the first error
    /// in the written order is the one reported. Fails with
    /// [`QueryError::LimitExceeded`] on a node more than `max_depth` `And` /
    /// `Or` levels deep (a leaf adds no level; `None` sets no cap), with
    /// [`QueryError::InvalidQuery`] on an `And` or `Or` with no child, and
    /// with [`QueryError::InvalidArgument`] on a leaf with a bad field name.
    /// A leaf that admits no value is [`Plan::Empty`]. `Eq` values, `In`
    /// values and `Range` bounds are of one indexable type by construction.
    pub(crate) fn admit(filter: &Filter, max_depth: Option<u32>) -> Result<Self> {
        Admission { max_depth }.plan(filter, 0)
    }
}

/// One filter's admission, under its depth cap. Admission and evaluation
/// recurse once per `And` / `Or` level, so the cap bounds their stack too: a
/// node over it fails before admission goes any deeper.
struct Admission {
    max_depth: Option<u32>,
}

impl Admission {
    /// The plan of `filter`, with `depth` `And` / `Or` levels above it.
    fn plan(&self, filter: &Filter, depth: u32) -> Result<Plan> {
        match filter {
            Filter::Eq { field, value } => Ok(Plan::TermRead(term(field, value.as_cell())?)),
            Filter::In { field, values } => {
                Ok(terms(field, values)?.map_or(Plan::Empty, Plan::TermReads))
            }
            Filter::Range { field, bounds } => {
                Ok(range(field, bounds)?.map_or(Plan::Empty, Plan::Scan))
            }
            Filter::Prefix { field, prefix } => TermRange::with_prefix(field.as_view(), prefix)
                .map(Plan::Scan)
                .map_err(|source| invalid_argument(field, source)),
            Filter::And(children) => {
                let (first, rest) =
                    self.children(children, depth, "`And` needs at least one child")?;
                Ok(Plan::And { first, rest })
            }
            Filter::Or(children) => {
                let (first, rest) =
                    self.children(children, depth, "`Or` needs at least one child")?;
                Ok(Plan::Or { first, rest })
            }
        }
    }

    /// The plans of a node's `children`, in the written order: the first, and
    /// the rest. The node adds one level to `depth`: over the cap, it fails
    /// before anything else; with no child, it is an invalid query, reported
    /// as `empty`.
    fn children(
        &self,
        children: &[Filter],
        depth: u32,
        empty: &'static str,
    ) -> Result<(Box<Plan>, Vec<Plan>)> {
        let depth = depth + 1;
        if let Some(max) = self.max_depth.filter(|&max| depth > max) {
            return Err(QueryError::LimitExceeded {
                limit: "filter depth",
                max,
            });
        }
        let [first, rest @ ..] = children else {
            return Err(QueryError::InvalidQuery { reason: empty });
        };
        let first = Box::new(self.plan(first, depth)?);
        let rest = rest
            .iter()
            .map(|child| self.plan(child, depth))
            .collect::<Result<_>>()?;
        Ok((first, rest))
    }
}

/// The terms of `field` between the bounds, in the run of their type, as
/// [`TermRange::between`] builds them: a bad field name fails as an `Eq`
/// leaf's would. Two unbounded sides are the whole run. `None` when the
/// bounds admit no value. The bounds are of one indexable type by
/// construction, so a type with no term cannot occur here, and a type
/// mismatch is a broken invariant, not the caller's error.
fn range(field: &CellName, bounds: &Bounds) -> Result<Option<TermRange>> {
    TermRange::between(
        field.as_view(),
        bounds.cell_type(),
        bounds.lower().map(CellValue::as_view),
        bounds.upper().map(CellValue::as_view),
    )
    .map_err(|error| match error {
        RangeError::Term(source) => invalid_argument(field, source),
        error @ RangeError::TypeMismatch { .. } => QueryError::Internal(Box::new(error)),
    })
}

/// The terms of `field IN values`, one per value, in the values' order
/// (ascending term key); `None` when there is no value, once the field's
/// name is checked. The values are of one indexable type by construction,
/// so only the name can fail.
fn terms(field: &CellName, values: &InValues) -> Result<Option<BTreeSet<IndexTerm>>> {
    match values.as_slice() {
        [] => IndexTerm::check_cell_name(field.as_view())
            .map(|()| None)
            .map_err(|source| invalid_argument(field, source)),
        values => values
            .iter()
            .map(|value| term(field, value))
            .collect::<Result<_>>()
            .map(Some),
    }
}

/// The term `field = value` is indexed under, built as seal builds it, so
/// queries and writes share one encoding.
fn term(field: &CellName, value: &CellValue) -> Result<IndexTerm> {
    IndexTerm::from_cell_name(field.as_view(), value.as_view())
        .map_err(|source| invalid_argument(field, source))
}

fn invalid_argument(field: &CellName, source: TermError) -> QueryError {
    QueryError::InvalidArgument {
        field: field.clone(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Bound::{self, Excluded, Included, Unbounded};

    use golemdb_cells::{
        CellNameRef, CellType, NotNan64, Width,
        marker::{Dec256, F64, I32, I64, I256, Str, U32, U64},
    };

    use super::*;
    use crate::EqValue;

    /// Admission with no depth cap; the depth tests set one.
    fn admit(filter: &Filter) -> Result<Plan> {
        Plan::admit(filter, None)
    }

    #[test]
    fn valid_leaf_resolves_to_its_index_term() {
        let field: CellName = CellNameRef::raw(b"price").into();
        let value = CellValue::from_u32(7);
        let expected = IndexTerm::from_str_name("price", value.as_view()).expect("valid term");
        assert_eq!(term(&field, &value).expect("valid leaf"), expected);
    }

    #[test]
    fn non_utf8_name_is_invalid_name() {
        let field: CellName = CellNameRef::raw(&[0xff]).into();
        let result = term(&field, &CellValue::from_u32(7));
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument {
                source: TermError::InvalidName,
                ..
            })
        ));
    }

    #[test]
    fn term_errors_are_kept_as_the_cause() {
        let bad_name: CellName = CellNameRef::raw(b"1bad").into();
        let result = term(&bad_name, &CellValue::from_u32(7));
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument {
                source: TermError::InvalidName,
                ..
            })
        ));

        let field: CellName = CellNameRef::raw(b"price").into();
        let result = term(&field, &CellValue::from_bytes(b"x"));
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument {
                source: TermError::UnsupportedType(CellType::Bytes),
                ..
            })
        ));
    }

    /// The terms of a non-empty `In` on `f`.
    fn in_terms(values: &InValues) -> Result<BTreeSet<IndexTerm>> {
        terms(&CellNameRef::raw(b"f").into(), values).map(|terms| terms.expect("values given"))
    }

    fn term_of(value: &CellValue) -> IndexTerm {
        IndexTerm::from_str_name("f", value.as_view()).expect("valid term")
    }

    #[test]
    fn in_terms_are_sorted_by_key_and_deduplicated() {
        // The set iterates, and so is read, in term key order, not native
        // order: -1 sorts before 1 once the sign bit is flipped, and "ab"
        // sorts before "b".
        let got = in_terms(&InValues::of(I32, [1, -1, 1])).expect("valid In");
        let expected = [-1, 1].map(|n| term_of(&CellValue::from_i32(n)));
        assert!(got.iter().eq(&expected), "{got:?}");

        let got = in_terms(&InValues::of(Str, ["b", "ab", "b", "ab"])).expect("valid In");
        let expected = ["ab", "b"].map(|s| term_of(&CellValue::from_str(s)));
        assert!(got.iter().eq(&expected), "{got:?}");
    }

    #[test]
    fn in_terms_follow_the_order_the_values_are_held_in() {
        // `InValues` holds its values in term key order, one term each.
        let f64 = |value| NotNan64::new(value).expect("not NaN");
        for values in [
            InValues::of(I32, [1, -1, 0, i32::MIN, i32::MAX, 1]),
            InValues::of(U32, [7, 0, u32::MAX]),
            InValues::of(Str, ["b", "ab", "a", "", "é", "ab"]),
            InValues::of(I256, [[0xff; 32], [0; 32], [0x7f; 32], [0x80; 32]]),
            InValues::of(Dec256, [[0xff; 32], [0; 32]]),
            InValues::of(F64, [f64(1.0), f64(-1.0), f64(f64::INFINITY), f64(-0.0)]),
        ] {
            let got = in_terms(&values).expect("valid In");
            assert!(
                got.iter()
                    .eq(&values.as_slice().iter().map(term_of).collect::<Vec<_>>()),
                "{values:?}"
            );
        }
    }

    #[test]
    fn in_with_one_value_reads_the_eq_term() {
        assert_eq!(
            in_terms(&InValues::of(U64, [9])).expect("valid In"),
            BTreeSet::from([term_of(&CellValue::from_u64(9))])
        );
    }

    #[test]
    fn an_in_with_no_value_is_empty_once_its_name_is_checked() {
        let plan = |field: &[u8]| {
            admit(&Filter::In {
                field: CellNameRef::raw(field).into(),
                values: InValues::of(U32, []),
            })
        };
        assert_eq!(plan(b"f").expect("valid In"), Plan::Empty);
        for field in [&b"1bad"[..], &[0xff]] {
            assert!(
                matches!(
                    plan(field),
                    Err(QueryError::InvalidArgument {
                        source: TermError::InvalidName,
                        ..
                    })
                ),
                "{field:?}"
            );
        }
    }

    #[test]
    fn an_in_field_is_checked_as_an_eq_field() {
        let field: CellName = CellNameRef::raw(b"1bad").into();
        let result = terms(&field, &InValues::of(U32, [1]));
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument {
                source: TermError::InvalidName,
                ..
            })
        ));
    }

    /// Admission of a `Range` on `field`.
    fn range_plan(field: &[u8], bounds: &Bounds) -> Result<Plan> {
        admit(&Filter::Range {
            field: CellNameRef::raw(field).into(),
            bounds: bounds.clone(),
        })
    }

    #[test]
    fn range_resolves_to_a_scan_of_its_run_or_to_empty() {
        let field: CellName = CellNameRef::raw(b"f").into();
        for bounds in [
            Bounds::new(I64, Included(-5), Excluded(5)),
            Bounds::new(I64, Excluded(-5), Unbounded),
            Bounds::new(I64, Unbounded, Included(5)),
            Bounds::new(I64, Included(5), Included(5)),
        ] {
            let expected = TermRange::between(
                field.as_view(),
                bounds.cell_type(),
                bounds.lower().map(CellValue::as_view),
                bounds.upper().map(CellValue::as_view),
            )
            .expect("valid range")
            .expect("non-empty range");
            assert_eq!(
                range_plan(b"f", &bounds).expect("valid Range"),
                Plan::Scan(expected),
                "{bounds:?}"
            );
        }
        for bounds in [
            Bounds::new(I64, Included(5), Included(-5)),
            Bounds::new(I64, Excluded(5), Excluded(-5)),
            Bounds::new(I64, Included(5), Excluded(5)),
            Bounds::new(I64, Excluded(5), Excluded(5)),
        ] {
            assert_eq!(
                range_plan(b"f", &bounds).expect("valid Range"),
                Plan::Empty,
                "{bounds:?}"
            );
        }
    }

    #[test]
    fn two_unbounded_sides_scan_the_whole_run() {
        let bounds = Bounds::new(I64, Unbounded, Unbounded);
        let expected = TermRange::between(
            CellNameRef::raw(b"f"),
            CellType::Int(Width::W8),
            Unbounded,
            Unbounded,
        )
        .expect("valid range")
        .expect("non-empty range");
        assert_eq!(
            range_plan(b"f", &bounds).expect("valid Range"),
            Plan::Scan(expected)
        );
    }

    #[test]
    fn a_range_field_is_checked_as_an_eq_field() {
        let bounded = Bounds::new(I64, Bound::Included(1), Unbounded);
        let whole_run = Bounds::new(I64, Unbounded, Unbounded);
        for (field, bounds) in [
            (&b"1bad"[..], &bounded),
            (&[0xff], &bounded),
            (b"1bad", &whole_run),
            (&[0xff], &whole_run),
        ] {
            assert!(
                matches!(
                    range_plan(field, bounds),
                    Err(QueryError::InvalidArgument {
                        source: TermError::InvalidName,
                        ..
                    })
                ),
                "{field:?}"
            );
        }
    }

    #[test]
    fn invalid_argument_names_the_field() {
        let field: CellName = CellNameRef::raw(b"1bad").into();
        let result = term(&field, &CellValue::from_u32(7));
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument { field: got, .. }) if got == field
        ));
    }

    /// Admission of `field` starting with `prefix`. Raw, so tests can also
    /// pass names the grammar rejects.
    fn prefix(field: &[u8], prefix: &str) -> Result<Plan> {
        admit(&Filter::Prefix {
            field: CellNameRef::raw(field).into(),
            prefix: prefix.to_owned(),
        })
    }

    #[test]
    fn a_prefix_resolves_to_a_scan_of_the_strings_starting_with_it() {
        for value in ["", "ab", "é"] {
            let expected =
                TermRange::with_prefix(CellNameRef::raw(b"f"), value).expect("valid prefix");
            assert_eq!(
                prefix(b"f", value).expect("valid Prefix"),
                Plan::Scan(expected),
                "{value:?}"
            );
        }
    }

    #[test]
    fn a_prefix_field_is_checked_like_any_leaf_field() {
        for field in [&b"1bad"[..], &[0xff]] {
            assert!(
                matches!(
                    prefix(field, "a"),
                    Err(QueryError::InvalidArgument {
                        source: TermError::InvalidName,
                        ..
                    })
                ),
                "{field:?}"
            );
        }
    }

    /// An `Eq` leaf on `field`, valid or not.
    fn eq_u32(field: &[u8], value: u32) -> Filter {
        Filter::Eq {
            field: CellNameRef::raw(field).into(),
            value: EqValue::of(U32, value),
        }
    }

    fn plan_of(filter: &Filter) -> Plan {
        admit(filter).expect("valid filter")
    }

    #[test]
    fn and_and_or_keep_their_children_in_the_written_order() {
        let (a, b, c) = (eq_u32(b"a", 1), eq_u32(b"b", 2), eq_u32(b"c", 3));
        let (plan_a, plan_b, plan_c) = (plan_of(&a), plan_of(&b), plan_of(&c));
        let and = Filter::And(vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(
            plan_of(&and),
            Plan::And {
                first: Box::new(plan_a.clone()),
                rest: vec![plan_b.clone(), plan_c.clone()],
            }
        );
        let or = Filter::Or(vec![c, a]);
        assert_eq!(
            plan_of(&or),
            Plan::Or {
                first: Box::new(plan_c),
                rest: vec![plan_a],
            }
        );
    }

    #[test]
    fn a_node_with_one_child_keeps_its_shape() {
        let leaf = eq_u32(b"a", 1);
        assert_eq!(
            plan_of(&Filter::And(vec![leaf.clone()])),
            Plan::And {
                first: Box::new(plan_of(&leaf)),
                rest: vec![],
            }
        );
    }

    #[test]
    fn nodes_nest() {
        let (a, b, c) = (eq_u32(b"a", 1), eq_u32(b"b", 2), eq_u32(b"c", 3));
        let filter = Filter::And(vec![Filter::Or(vec![a.clone(), b.clone()]), c.clone()]);
        assert_eq!(
            plan_of(&filter),
            Plan::And {
                first: Box::new(Plan::Or {
                    first: Box::new(plan_of(&a)),
                    rest: vec![plan_of(&b)],
                }),
                rest: vec![plan_of(&c)],
            }
        );
    }

    #[test]
    fn an_and_or_an_or_with_no_child_is_an_invalid_query() {
        for filter in [
            Filter::And(vec![]),
            Filter::Or(vec![]),
            Filter::And(vec![eq_u32(b"a", 1), Filter::Or(vec![])]),
            Filter::Or(vec![Filter::And(vec![Filter::And(vec![])])]),
        ] {
            assert!(
                matches!(admit(&filter), Err(QueryError::InvalidQuery { .. })),
                "{filter:?}"
            );
        }
    }

    #[test]
    fn the_first_error_in_the_written_order_is_reported() {
        let bad_name = |field: &[u8]| eq_u32(field, 1);
        let filter = Filter::And(vec![
            eq_u32(b"a", 1),
            Filter::Or(vec![bad_name(b"1first"), Filter::And(vec![])]),
            bad_name(b"2second"),
        ]);
        assert!(matches!(
            admit(&filter),
            Err(QueryError::InvalidArgument { field, .. }) if field.as_bytes() == b"1first"
        ));
        let filter = Filter::Or(vec![Filter::And(vec![]), bad_name(b"1first")]);
        assert!(matches!(
            admit(&filter),
            Err(QueryError::InvalidQuery { .. })
        ));
    }

    /// `levels` `And` / `Or` levels over one leaf, alternating: the nested
    /// node is the last child of each `And` and the only one of each `Or`, so
    /// both ways a child is admitted are taken.
    fn nested(levels: u32) -> Filter {
        (0..levels).fold(eq_u32(b"a", 1), |filter, level| {
            if level % 2 == 0 {
                Filter::And(vec![eq_u32(b"b", 2), filter])
            } else {
                Filter::Or(vec![filter])
            }
        })
    }

    /// Whether `result` is the failure of a node over a depth cap of `cap`.
    fn over_the_cap(result: &Result<Plan>, cap: u32) -> bool {
        matches!(
            result,
            Err(QueryError::LimitExceeded { limit: "filter depth", max }) if *max == cap
        )
    }

    #[test]
    fn a_filter_nests_up_to_its_depth_cap() {
        for cap in [1, 2, 3, 64] {
            let at_the_cap = nested(cap);
            assert_eq!(
                Plan::admit(&at_the_cap, Some(cap)).expect("nests within the cap"),
                plan_of(&at_the_cap),
                "{cap}"
            );
            assert!(
                over_the_cap(&Plan::admit(&nested(cap + 1), Some(cap)), cap),
                "{cap}"
            );
        }
    }

    #[test]
    fn depth_counts_the_levels_of_the_longest_path() {
        // A leaf adds no level: it is admitted under a cap of 0, a node is not.
        let leaf = eq_u32(b"a", 1);
        Plan::admit(&leaf, Some(0)).expect("a leaf has no depth");
        assert!(over_the_cap(
            &Plan::admit(&Filter::Or(vec![leaf]), Some(0)),
            0
        ));
        // Siblings add no level, however many.
        let wide = Filter::Or((0..1000).map(|n| eq_u32(b"a", n)).collect());
        Plan::admit(&wide, Some(1)).expect("one level");
        // The deepest child sets the depth, wherever it is written.
        for uneven in [
            Filter::And(vec![nested(3), eq_u32(b"b", 1), nested(1)]),
            Filter::Or(vec![nested(1), nested(3)]),
        ] {
            Plan::admit(&uneven, Some(4)).expect("four levels");
            assert!(
                over_the_cap(&Plan::admit(&uneven, Some(3)), 3),
                "{uneven:?}"
            );
        }
    }

    #[test]
    fn with_no_cap_the_depth_is_not_checked() {
        Plan::admit(&nested(100), None).expect("no cap");
    }

    #[test]
    fn the_depth_is_checked_in_the_written_order() {
        let too_deep = nested(3);
        let bad_name = eq_u32(b"1bad", 1);
        // Whichever error is written first is the one reported.
        let filter = Filter::And(vec![bad_name.clone(), too_deep.clone()]);
        assert!(matches!(
            Plan::admit(&filter, Some(2)),
            Err(QueryError::InvalidArgument { .. })
        ));
        let filter = Filter::And(vec![too_deep, bad_name]);
        assert!(over_the_cap(&Plan::admit(&filter, Some(2)), 2));
        // A node over the cap fails before its children are looked at, so
        // before it is found to have none.
        let filter = Filter::And(vec![Filter::Or(vec![])]);
        assert!(over_the_cap(&Plan::admit(&filter, Some(1)), 1));
    }

    #[test]
    fn admission_stops_at_the_cap_however_deep_the_tree() {
        // Admitted in full, 5,000 levels would overflow a test thread's stack
        // in either build; under a cap, admission goes one level past it.
        let deep = nested(5_000);
        assert!(over_the_cap(&Plan::admit(&deep, Some(64)), 64));
    }
}
