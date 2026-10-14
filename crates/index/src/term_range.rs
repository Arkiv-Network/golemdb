use std::ops::Bound;

use golemdb_cells::{CellNameRef, CellType, CellValueRef};

use crate::{IndexTerm, TermError, term::name_str};

/// A range of index keys inside one name/type run, for an ordered scan with
/// [`crate::Index::terms`].
///
/// Every key in the range starts with the run's prefix, so a scan never
/// leaks into another type under the same name, nor into a longer name
/// (`ab` when scanning `a`). Within the run, key order is value order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermRange {
    lower: Bound<Vec<u8>>,
    upper: Bound<Vec<u8>>,
}

/// Why bounds make no [`TermRange`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    #[error("a range needs at least one bound")]
    NoBound,
    #[error("range bounds of two types: {lower:?} and {upper:?}")]
    TypeMismatch { lower: CellType, upper: CellType },
    #[error("range lower bound is above its upper bound")]
    Inverted,
    /// The name, or the bounds' type, has no index term.
    #[error(transparent)]
    Term(#[from] TermError),
}

impl TermRange {
    /// The terms of `name` whose value lies between `lower` and `upper`, in
    /// the run of the bounds' type. A bounded side is the term of its value;
    /// an unbounded side reaches the end of the run.
    ///
    /// The bounds' shape is checked first, without building a term:
    /// [`RangeError::NoBound`], then [`RangeError::TypeMismatch`]. Then the
    /// name and the type, as [`IndexTerm::for_cell_name`] does
    /// ([`RangeError::Term`]), and last [`RangeError::Inverted`] on the keys.
    pub fn between(
        name: CellNameRef<'_>,
        lower: Bound<CellValueRef<'_>>,
        upper: Bound<CellValueRef<'_>>,
    ) -> Result<Self, RangeError> {
        let ty = match (bound_value(lower), bound_value(upper)) {
            (None, None) => return Err(RangeError::NoBound),
            (Some(low), Some(high)) if low.cell_type() != high.cell_type() => {
                return Err(RangeError::TypeMismatch {
                    lower: low.cell_type(),
                    upper: high.cell_type(),
                });
            }
            (Some(value), _) | (None, Some(value)) => value.cell_type(),
        };
        let name = name_str(name)?;
        let run = IndexTerm::prefix(name, ty)?;
        let term = |value| IndexTerm::for_value(name, value).map(IndexTerm::into_bytes);
        let lower = match lower {
            Bound::Included(value) => Bound::Included(term(value)?),
            Bound::Excluded(value) => Bound::Excluded(term(value)?),
            Bound::Unbounded => Bound::Included(run.clone()),
        };
        let upper = match upper {
            Bound::Included(value) => Bound::Included(term(value)?),
            Bound::Excluded(value) => Bound::Excluded(term(value)?),
            Bound::Unbounded => past(run),
        };
        // The keys share the run's prefix, so key order is value order. This
        // is the check a storage scan makes: one never fails on these bounds.
        if let (
            Bound::Included(low) | Bound::Excluded(low),
            Bound::Included(high) | Bound::Excluded(high),
        ) = (&lower, &upper)
            && low > high
        {
            return Err(RangeError::Inverted);
        }
        Ok(Self { lower, upper })
    }

    /// The key bounds, usable as a `RangeBounds<[u8]>`.
    pub fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        (
            self.lower.as_ref().map(Vec::as_slice),
            self.upper.as_ref().map(Vec::as_slice),
        )
    }
}

fn bound_value<T>(bound: Bound<T>) -> Option<T> {
    match bound {
        Bound::Included(value) | Bound::Excluded(value) => Some(value),
        Bound::Unbounded => None,
    }
}

/// The upper bound just past every key that starts with `prefix`: excluding
/// the least key greater than all of them, or unbounded when `prefix` is all
/// `0xff`.
fn past(mut prefix: Vec<u8>) -> Bound<Vec<u8>> {
    while let Some(last) = prefix.pop() {
        if let Some(next) = last.checked_add(1) {
            prefix.push(next);
            return Bound::Excluded(prefix);
        }
    }
    Bound::Unbounded
}

#[cfg(test)]
mod tests {
    use std::ops::{
        Bound::{Excluded, Included, Unbounded},
        RangeBounds,
    };

    use golemdb_cells::{CellValue, Width};

    use super::*;

    /// Raw, so tests can also pass names the grammar rejects.
    fn between(
        name: &[u8],
        lower: Bound<CellValue>,
        upper: Bound<CellValue>,
    ) -> Result<TermRange, RangeError> {
        TermRange::between(
            CellNameRef::raw(name),
            lower.as_ref().map(CellValue::as_view),
            upper.as_ref().map(CellValue::as_view),
        )
    }

    fn range(name: &str, lower: Bound<&CellValue>, upper: Bound<&CellValue>) -> TermRange {
        between(name.as_bytes(), lower.cloned(), upper.cloned()).expect("valid range")
    }

    fn term(name: &str, value: &CellValue) -> IndexTerm {
        IndexTerm::for_value(name, value.as_view()).expect("valid term")
    }

    /// The terms of `all`, in key order, that fall within `range`.
    fn within<'a>(range: &TermRange, all: &'a [IndexTerm]) -> Vec<&'a IndexTerm> {
        let mut sorted: Vec<_> = all.iter().collect();
        sorted.sort();
        sorted
            .into_iter()
            .filter(|term| range.bounds().contains(term.as_bytes()))
            .collect()
    }

    const U32: CellType = CellType::Uint(Width::W4);

    fn u32(n: u32) -> CellValue {
        CellValue::from_u32(n)
    }

    // Keys of the `u32` run of `a`: name, 0x00, type tag 0x8c, then the value.
    const RUN_START: &[u8] = &[b'a', 0, 0x8c];
    const ONE: &[u8] = &[b'a', 0, 0x8c, 0, 0, 0, 1];
    const SEVEN: &[u8] = &[b'a', 0, 0x8c, 0, 0, 0, 7];
    /// The run's prefix with the type tag incremented: past every `u32` of `a`.
    const RUN_END: &[u8] = &[b'a', 0, 0x8d];

    #[test]
    fn included_bounds_are_the_terms_of_their_values() {
        let range = between(b"a", Included(u32(1)), Included(u32(7))).unwrap();
        assert_eq!(range.bounds(), (Included(ONE), Included(SEVEN)));
    }

    #[test]
    fn excluded_bounds_are_the_terms_of_their_values() {
        let range = between(b"a", Excluded(u32(1)), Excluded(u32(7))).unwrap();
        assert_eq!(range.bounds(), (Excluded(ONE), Excluded(SEVEN)));
    }

    #[test]
    fn an_unbounded_lower_side_starts_at_the_run() {
        let range = between(b"a", Unbounded, Included(u32(7))).unwrap();
        assert_eq!(range.bounds(), (Included(RUN_START), Included(SEVEN)));
    }

    #[test]
    fn an_unbounded_upper_side_ends_past_the_run() {
        let range = between(b"a", Included(u32(1)), Unbounded).unwrap();
        assert_eq!(range.bounds(), (Included(ONE), Excluded(RUN_END)));
    }

    #[test]
    fn a_range_needs_a_bound() {
        assert_eq!(
            between(b"a", Unbounded, Unbounded),
            Err(RangeError::NoBound)
        );
    }

    #[test]
    fn bounds_of_two_types_are_a_mismatch() {
        let result = between(b"a", Included(u32(1)), Excluded(CellValue::from_i32(1)));
        let mismatch = RangeError::TypeMismatch {
            lower: U32,
            upper: CellType::Int(Width::W4),
        };
        assert_eq!(result, Err(mismatch));
    }

    #[test]
    fn inverted_bounds_are_rejected() {
        for (lower, upper) in [
            (Included(u32(7)), Included(u32(1))),
            (Excluded(u32(7)), Excluded(u32(1))),
            (Included(u32(1)), Excluded(u32(0))),
        ] {
            let result = between(b"a", lower.clone(), upper.clone());
            assert_eq!(result, Err(RangeError::Inverted), "{lower:?}..{upper:?}");
        }
    }

    #[test]
    fn equal_bounds_with_an_excluded_side_are_empty() {
        let (run, neighbours) = neighbourhood();
        let all = [run.clone(), neighbours].concat();
        let seven = u32(7);
        assert_eq!(
            within(&range("a", Included(&seven), Included(&seven)), &all),
            [&run[2]]
        );
        for (lower, upper) in [
            (Included(&seven), Excluded(&seven)),
            (Excluded(&seven), Included(&seven)),
            (Excluded(&seven), Excluded(&seven)),
        ] {
            let empty = range("a", lower, upper);
            assert!(within(&empty, &all).is_empty(), "{lower:?}..{upper:?}");
        }
    }

    #[test]
    fn a_name_that_is_not_utf8_is_invalid() {
        let result = between(&[0xff], Included(u32(1)), Unbounded);
        assert_eq!(result, Err(RangeError::Term(TermError::InvalidName)));
    }

    #[test]
    fn a_name_outside_the_grammar_is_invalid() {
        let result = between(b"1a", Unbounded, Included(u32(1)));
        assert_eq!(result, Err(RangeError::Term(TermError::InvalidName)));
    }

    #[test]
    fn bytes_has_no_run() {
        let result = between(b"a", Included(CellValue::from_bytes(b"x")), Unbounded);
        let unsupported = TermError::UnsupportedType(CellType::Bytes);
        assert_eq!(result, Err(RangeError::Term(unsupported)));
    }

    #[test]
    fn the_shape_is_checked_before_the_name_and_inversion_after() {
        assert_eq!(
            between(b"1a", Unbounded, Unbounded),
            Err(RangeError::NoBound)
        );
        assert!(matches!(
            between(b"1a", Included(u32(1)), Included(CellValue::from_u64(1))),
            Err(RangeError::TypeMismatch { .. })
        ));
        assert_eq!(
            between(b"1a", Included(u32(7)), Included(u32(1))),
            Err(RangeError::Term(TermError::InvalidName))
        );
    }

    /// `u32` terms of `a` with neighbours on every side: other types under `a`,
    /// and names that `a` is a prefix of or that sort just after it.
    fn neighbourhood() -> (Vec<IndexTerm>, Vec<IndexTerm>) {
        let run: Vec<_> = [0, 1, 7, u32::MAX]
            .map(|n| term("a", &CellValue::from_u32(n)))
            .into();
        let neighbours = vec![
            term("a", &CellValue::from_bool(true)),
            term("a", &CellValue::from_str("")),
            term("a", &CellValue::from_u64(0)),
            term("a", &CellValue::from_u64(u64::MAX)),
            term("a", &CellValue::from_i32(0)),
            term("ab", &CellValue::from_u32(0)),
            term("a0", &CellValue::from_u32(7)),
            term("b", &CellValue::from_u32(1)),
            term("A", &CellValue::from_u32(1)),
        ];
        (run, neighbours)
    }

    #[test]
    fn unbounded_sides_stay_in_the_run() {
        let (run, neighbours) = neighbourhood();
        let all = [run.clone(), neighbours].concat();
        let (min, max) = (u32(0), u32(u32::MAX));
        for range in [
            range("a", Unbounded, Included(&max)),
            range("a", Included(&min), Unbounded),
        ] {
            assert!(within(&range, &all).into_iter().eq(&run), "{range:?}");
        }
    }

    #[test]
    fn bounds_are_values_in_the_run() {
        let (run, neighbours) = neighbourhood();
        let all = [run.clone(), neighbours].concat();
        let (one, seven) = (u32(1), u32(7));
        for (lower, upper, expected) in [
            (Included(&one), Included(&seven), &run[1..3]),
            (Excluded(&one), Included(&seven), &run[2..3]),
            (Included(&one), Excluded(&seven), &run[1..2]),
            (Excluded(&one), Excluded(&seven), &run[..0]),
            (Included(&one), Unbounded, &run[1..]),
            (Excluded(&one), Unbounded, &run[2..]),
            (Unbounded, Included(&seven), &run[..3]),
            (Unbounded, Excluded(&seven), &run[..2]),
        ] {
            let got = within(&range("a", lower, upper), &all);
            assert!(got.into_iter().eq(expected), "{lower:?}..{upper:?}");
        }
    }

    #[test]
    fn the_empty_string_is_the_least_str() {
        let (empty, a) = (CellValue::from_str(""), CellValue::from_str("a"));
        let all = [term("s", &empty), term("s", &a)];
        let at_most_empty = range("s", Unbounded, Included(&empty));
        assert!(within(&at_most_empty, &all).into_iter().eq(&all[..1]));
        // The excluded empty string is the run's own prefix: nothing lies below.
        let below_empty = range("s", Unbounded, Excluded(&empty));
        assert!(within(&below_empty, &all).is_empty());
        let above_empty = range("s", Excluded(&empty), Unbounded);
        assert!(within(&above_empty, &all).into_iter().eq(&all[1..]));
    }

    #[test]
    fn past_skips_trailing_0xff_bytes() {
        assert_eq!(past(vec![1, 2]), Bound::Excluded(vec![1, 3]));
        assert_eq!(past(vec![1, 0xff, 0xff]), Bound::Excluded(vec![2]));
        assert_eq!(past(vec![0xff, 0xff]), Bound::Unbounded);
        assert_eq!(past(vec![]), Bound::Unbounded);
    }
}
