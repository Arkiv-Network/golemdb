//! [`Plan`]: a filter after admission. Building it validates the whole filter
//! before any state read and resolves its leaves to the index work they need.
//! Pure; no storage access.

use std::{collections::BTreeSet, ops::Bound};

use golemdb_cells::{CellName, CellValue};
use golemdb_index::{IndexTerm, RangeError, TermError, TermRange};

use crate::{Filter, QueryError, Result};

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
    /// each: the union of their postings.
    Scan(TermRange),
    /// A leaf no record can match, an `In` with no value or a `Range` whose
    /// bounds admit none: nothing is read.
    Empty,
}

impl TryFrom<&Filter> for Plan {
    type Error = QueryError;

    /// Admission: fails with [`QueryError::InvalidArgument`] on a leaf with no
    /// valid index term, and with [`QueryError::InvalidQuery`] on a malformed
    /// leaf (an `In` whose values differ in type; a `Range` with no bound, or
    /// bounds of two types). A leaf that admits no value is [`Plan::Empty`].
    fn try_from(filter: &Filter) -> Result<Self> {
        match filter {
            Filter::Eq { field, value } => Ok(Self::TermRead(term(field, value)?)),
            Filter::In { field, values } => {
                Ok(terms(field, values)?.map_or(Self::Empty, Self::TermReads))
            }
            Filter::Range {
                field,
                lower,
                upper,
            } => Ok(range(field, lower, upper)?.map_or(Self::Empty, Self::Scan)),
        }
    }
}

/// The terms of `field` between `lower` and `upper`, checked as
/// [`TermRange::between`] checks them: a field or type with no index term
/// fails as an `Eq` leaf's would, and a malformed range is an invalid query.
/// `None` when the bounds admit no value.
fn range(
    field: &CellName,
    lower: &Bound<CellValue>,
    upper: &Bound<CellValue>,
) -> Result<Option<TermRange>> {
    TermRange::between(
        field.as_view(),
        lower.as_ref().map(CellValue::as_view),
        upper.as_ref().map(CellValue::as_view),
    )
    .map_err(|error| match error {
        RangeError::Term(source) => invalid_argument(field, source),
        RangeError::NoBound => QueryError::InvalidQuery {
            reason: "`Range` needs at least one bound",
        },
        RangeError::TypeMismatch { .. } => QueryError::InvalidQuery {
            reason: "`Range` bounds must have one type",
        },
    })
}

/// The terms of `field IN values`, sorted by key and deduplicated; `None`
/// when there are no values. The list's shape is checked first, without
/// building a term, then each value as an `Eq` leaf's would be. With no
/// value, the field's name is still checked.
fn terms(field: &CellName, values: &[CellValue]) -> Result<Option<BTreeSet<IndexTerm>>> {
    let [first, rest @ ..] = values else {
        IndexTerm::check_cell_name(field.as_view())
            .map_err(|source| invalid_argument(field, source))?;
        return Ok(None);
    };
    if rest
        .iter()
        .any(|value| value.cell_type() != first.cell_type())
    {
        return Err(QueryError::InvalidQuery {
            reason: "`In` values must all have one type",
        });
    }
    values
        .iter()
        .map(|value| term(field, value))
        .collect::<Result<_>>()
        .map(Some)
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
    use golemdb_cells::{CellNameRef, CellType};

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
    fn in_terms(values: &[CellValue]) -> Result<BTreeSet<IndexTerm>> {
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
        let (minus, plus) = (CellValue::from_i32(-1), CellValue::from_i32(1));
        let got = in_terms(&[plus.clone(), minus.clone(), plus.clone()]).expect("valid In");
        assert!(
            got.iter().eq([&term_of(&minus), &term_of(&plus)]),
            "{got:?}"
        );

        let (ab, b) = (CellValue::from_str("ab"), CellValue::from_str("b"));
        let got = in_terms(&[b.clone(), ab.clone(), b.clone(), ab.clone()]).expect("valid In");
        assert!(got.iter().eq([&term_of(&ab), &term_of(&b)]), "{got:?}");
    }

    #[test]
    fn in_with_one_value_reads_the_eq_term() {
        let value = CellValue::from_u64(9);
        assert_eq!(
            in_terms(std::slice::from_ref(&value)).expect("valid In"),
            BTreeSet::from([term_of(&value)])
        );
    }

    #[test]
    fn malformed_in_is_an_invalid_query() {
        for values in [
            vec![CellValue::from_i32(1), CellValue::from_i64(1)],
            vec![
                CellValue::from_u32(1),
                CellValue::from_u32(2),
                CellValue::from_dec32_unscaled(3),
            ],
        ] {
            assert!(
                matches!(in_terms(&values), Err(QueryError::InvalidQuery { .. })),
                "{values:?}"
            );
        }
    }

    #[test]
    fn in_shape_is_checked_before_any_value() {
        // A bytes value has no term, but the mixed types are reported first.
        let result = in_terms(&[CellValue::from_str("x"), CellValue::from_bytes(b"x")]);
        assert!(matches!(result, Err(QueryError::InvalidQuery { .. })));
    }

    #[test]
    fn an_in_with_no_value_is_empty_once_its_name_is_checked() {
        let plan = |field: &[u8]| {
            Plan::try_from(&Filter::In {
                field: CellNameRef::raw(field).into(),
                values: vec![],
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
    fn in_values_are_checked_as_eq_values() {
        let result = in_terms(&[CellValue::from_bytes(b"x"), CellValue::from_bytes(b"y")]);
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument {
                source: TermError::UnsupportedType(CellType::Bytes),
                ..
            })
        ));
        let field: CellName = CellNameRef::raw(b"1bad").into();
        let result = terms(&field, &[CellValue::from_u32(1)]);
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument {
                source: TermError::InvalidName,
                ..
            })
        ));
    }

    #[test]
    fn range_resolves_to_a_scan_of_its_run_or_to_empty() {
        let field: CellName = CellNameRef::raw(b"f").into();
        let (low, high) = (CellValue::from_i64(-5), CellValue::from_i64(5));
        let plan = |lower: Bound<&CellValue>, upper: Bound<&CellValue>| {
            let filter = Filter::Range {
                field: field.clone(),
                lower: lower.cloned(),
                upper: upper.cloned(),
            };
            Plan::try_from(&filter).expect("valid Range")
        };
        for (lower, upper) in [
            (Bound::Included(&low), Bound::Excluded(&high)),
            (Bound::Excluded(&low), Bound::Unbounded),
            (Bound::Unbounded, Bound::Included(&high)),
            (Bound::Included(&high), Bound::Included(&high)),
        ] {
            let expected = TermRange::between(
                field.as_view(),
                lower.map(CellValue::as_view),
                upper.map(CellValue::as_view),
            )
            .expect("valid range")
            .expect("non-empty range");
            assert_eq!(
                plan(lower, upper),
                Plan::Scan(expected),
                "{lower:?}..{upper:?}"
            );
        }
        for (lower, upper) in [
            (Bound::Included(&high), Bound::Included(&low)),
            (Bound::Excluded(&high), Bound::Excluded(&low)),
            (Bound::Included(&high), Bound::Excluded(&high)),
            (Bound::Excluded(&high), Bound::Excluded(&high)),
        ] {
            assert_eq!(plan(lower, upper), Plan::Empty, "{lower:?}..{upper:?}");
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
}
