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

impl TermRange {
    /// The terms of `name` and type `ty` whose value lies between `lower`
    /// and `upper`. A bounded side is the term of its value; an unbounded
    /// side reaches the end of the run.
    ///
    /// Fails as [`IndexTerm::for_cell_name`] does on the name or the type,
    /// and with [`TermError::TypeMismatch`] on a bound of another type.
    /// Inverted bounds are not rejected here: scanning them fails.
    pub fn between(
        name: CellNameRef<'_>,
        ty: CellType,
        lower: Bound<CellValueRef<'_>>,
        upper: Bound<CellValueRef<'_>>,
    ) -> Result<Self, TermError> {
        let name = name_str(name)?;
        let run = IndexTerm::prefix(name, ty)?;
        let term = |value: CellValueRef<'_>| {
            if value.cell_type() != ty {
                return Err(TermError::TypeMismatch {
                    expected: ty,
                    actual: value.cell_type(),
                });
            }
            Ok(IndexTerm::for_value(name, value)?.into_bytes())
        };
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
        ty: CellType,
        lower: Bound<CellValue>,
        upper: Bound<CellValue>,
    ) -> Result<TermRange, TermError> {
        TermRange::between(
            CellNameRef::raw(name),
            ty,
            lower.as_ref().map(CellValue::as_view),
            upper.as_ref().map(CellValue::as_view),
        )
    }

    fn range(
        name: &str,
        ty: CellType,
        lower: Bound<&CellValue>,
        upper: Bound<&CellValue>,
    ) -> TermRange {
        between(name.as_bytes(), ty, lower.cloned(), upper.cloned()).expect("valid range")
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
        let range = between(b"a", U32, Included(u32(1)), Included(u32(7))).unwrap();
        assert_eq!(range.bounds(), (Included(ONE), Included(SEVEN)));
    }

    #[test]
    fn excluded_bounds_are_the_terms_of_their_values() {
        let range = between(b"a", U32, Excluded(u32(1)), Excluded(u32(7))).unwrap();
        assert_eq!(range.bounds(), (Excluded(ONE), Excluded(SEVEN)));
    }

    #[test]
    fn an_unbounded_lower_side_starts_at_the_run() {
        let range = between(b"a", U32, Unbounded, Included(u32(7))).unwrap();
        assert_eq!(range.bounds(), (Included(RUN_START), Included(SEVEN)));
    }

    #[test]
    fn an_unbounded_upper_side_ends_past_the_run() {
        let range = between(b"a", U32, Included(u32(1)), Unbounded).unwrap();
        assert_eq!(range.bounds(), (Included(ONE), Excluded(RUN_END)));
    }

    #[test]
    fn a_range_with_no_bound_is_the_whole_run() {
        let range = between(b"a", U32, Unbounded, Unbounded).unwrap();
        assert_eq!(range.bounds(), (Included(RUN_START), Excluded(RUN_END)));
    }

    #[test]
    fn inverted_bounds_are_not_checked() {
        // Scanning them fails in storage instead.
        let range = between(b"a", U32, Included(u32(7)), Included(u32(1))).unwrap();
        assert_eq!(range.bounds(), (Included(SEVEN), Included(ONE)));
    }

    #[test]
    fn a_name_that_is_not_utf8_is_invalid() {
        let result = between(&[0xff], U32, Unbounded, Unbounded);
        assert_eq!(result, Err(TermError::InvalidName));
    }

    #[test]
    fn a_name_outside_the_grammar_is_invalid() {
        let result = between(b"1a", U32, Unbounded, Unbounded);
        assert_eq!(result, Err(TermError::InvalidName));
    }

    #[test]
    fn bytes_has_no_run() {
        let result = between(b"a", CellType::Bytes, Unbounded, Unbounded);
        assert_eq!(result, Err(TermError::UnsupportedType(CellType::Bytes)));
    }

    #[test]
    fn a_lower_bound_of_another_type_is_a_mismatch() {
        let result = between(b"a", U32, Included(CellValue::from_u64(1)), Unbounded);
        let mismatch = TermError::TypeMismatch {
            expected: U32,
            actual: CellType::Uint(Width::W8),
        };
        assert_eq!(result, Err(mismatch));
    }

    #[test]
    fn an_upper_bound_of_another_type_is_a_mismatch() {
        let result = between(b"a", U32, Unbounded, Excluded(CellValue::from_i32(1)));
        let mismatch = TermError::TypeMismatch {
            expected: U32,
            actual: CellType::Int(Width::W4),
        };
        assert_eq!(result, Err(mismatch));
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
    fn an_unbounded_range_covers_exactly_its_run() {
        let (run, neighbours) = neighbourhood();
        let all = [run.clone(), neighbours].concat();
        let range = range("a", U32, Unbounded, Unbounded);
        assert!(within(&range, &all).into_iter().eq(&run));
    }

    #[test]
    fn bounds_are_values_in_the_run() {
        let (run, neighbours) = neighbourhood();
        let all = [run.clone(), neighbours].concat();
        let (one, seven) = (CellValue::from_u32(1), CellValue::from_u32(7));
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
            let got = within(&range("a", U32, lower, upper), &all);
            assert!(got.into_iter().eq(expected), "{lower:?}..{upper:?}");
        }
    }

    #[test]
    fn the_empty_string_is_the_least_str() {
        let (empty, a) = (CellValue::from_str(""), CellValue::from_str("a"));
        let all = [term("s", &empty), term("s", &a)];
        let ty = CellType::Str;
        let at_most_empty = range("s", ty, Unbounded, Included(&empty));
        assert!(within(&at_most_empty, &all).into_iter().eq(&all[..1]));
        // The excluded empty string is the run's own prefix: nothing lies below.
        let below_empty = range("s", ty, Unbounded, Excluded(&empty));
        assert!(within(&below_empty, &all).is_empty());
        let above_empty = range("s", ty, Excluded(&empty), Unbounded);
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
