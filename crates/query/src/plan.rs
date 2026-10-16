//! [`Plan`]: a filter after admission. Building it validates the whole filter
//! before any state read and resolves its leaves to the index work they need.
//! Pure; no storage access.

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
}

impl TryFrom<&Filter> for Plan {
    type Error = QueryError;

    /// Admission: fails with [`QueryError::InvalidArgument`] on a leaf with no
    /// valid index term (a bad field name, or an `Eq` on `bytes`). A leaf
    /// that admits no value is [`Plan::Empty`]. `In` values and `Range`
    /// bounds are of one indexable type by construction.
    fn try_from(filter: &Filter) -> Result<Self> {
        match filter {
            Filter::Eq { field, value } => Ok(Self::TermRead(term(field, value)?)),
            Filter::In { field, values } => {
                Ok(terms(field, values)?.map_or(Self::Empty, Self::TermReads))
            }
            Filter::Range { field, bounds } => {
                Ok(range(field, bounds)?.map_or(Self::Empty, Self::Scan))
            }
            Filter::Prefix { field, prefix } => TermRange::with_prefix(field.as_view(), prefix)
                .map(Self::Scan)
                .map_err(|source| invalid_argument(field, source)),
        }
    }
}

/// The terms of `field` between the bounds, in the run of their type, as
/// [`TermRange::between`] builds them: a bad field name fails as an `Eq`
/// leaf's would. Two unbounded sides are the whole run. `None` when the
/// bounds admit no value. The bounds are of one indexable type by
/// construction, so a type mismatch or a type with no term cannot occur
/// here.
fn range(field: &CellName, bounds: &Bounds) -> Result<Option<TermRange>> {
    TermRange::between(
        field.as_view(),
        bounds.cell_type(),
        bounds.lower().map(CellValue::as_view),
        bounds.upper().map(CellValue::as_view),
    )
    .map_err(|error| match error {
        RangeError::Term(source) => invalid_argument(field, source),
        RangeError::TypeMismatch { .. } => QueryError::InvalidQuery {
            reason: "`Range` bounds must have the range's type",
        },
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
            Plan::try_from(&Filter::In {
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
        Plan::try_from(&Filter::Range {
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
        let bounds: Bounds = Bounds::new(I64, Bound::Included(1), Unbounded);
        for field in [&b"1bad"[..], &[0xff]] {
            assert!(
                matches!(
                    range_plan(field, &bounds),
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
        Plan::try_from(&Filter::Prefix {
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
}
