use std::ops::Bound;

use golemdb_cells::{CellNameRef, CellType, CellValue, CellValueRef};

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
    /// A bound of another type than the range's.
    #[error("a range of {expected:?} has a bound of {actual:?}")]
    TypeMismatch {
        expected: CellType,
        actual: CellType,
    },
    /// The name, or the range's type, has no index term.
    #[error(transparent)]
    Term(#[from] TermError),
}

impl TermRange {
    /// The terms of `name` of type `ty` whose value lies between `lower` and
    /// `upper`. A bounded side is the term of its value; an unbounded side
    /// reaches the end of the run, so two unbounded sides are the whole run.
    /// `None` when the keys admit no term: the lower above the upper, or
    /// equal to it with a side excluded. Such a range matches nothing, and
    /// needs no scan.
    ///
    /// The bounds' types are checked first, without building a term
    /// ([`RangeError::TypeMismatch`]). Then the name and the type, as
    /// [`IndexTerm::from_cell_name`] does ([`RangeError::Term`]), so an empty
    /// range with a bad name still fails.
    pub fn between(
        name: CellNameRef<'_>,
        ty: CellType,
        lower: Bound<CellValueRef<'_>>,
        upper: Bound<CellValueRef<'_>>,
    ) -> Result<Option<Self>, RangeError> {
        if let Some(actual) = [bound_value(lower), bound_value(upper)]
            .into_iter()
            .flatten()
            .map(|value| value.cell_type())
            .find(|&actual| actual != ty)
        {
            return Err(RangeError::TypeMismatch {
                expected: ty,
                actual,
            });
        }
        let name = name_str(name)?;
        let run = IndexTerm::prefix(name, ty)?;
        let term = |value| IndexTerm::from_str_name(name, value).map(IndexTerm::into_bytes);
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
        // The keys share the run's prefix, so key order is value order.
        let empty = match (&lower, &upper) {
            (Bound::Included(low), Bound::Included(high)) => low > high,
            (
                Bound::Included(low) | Bound::Excluded(low),
                Bound::Included(high) | Bound::Excluded(high),
            ) => low >= high,
            (Bound::Unbounded, _) | (_, Bound::Unbounded) => false,
        };
        Ok((!empty).then_some(Self { lower, upper }))
    }

    /// The `str` terms of `name` whose value starts with `prefix`, byte by
    /// byte in UTF-8. The empty string is the least `str`, so it starts the
    /// whole run.
    ///
    /// The name is checked as [`IndexTerm::from_cell_name`] checks it. Every
    /// such range holds at least the prefix's own term, so it is never empty.
    pub fn with_prefix(name: CellNameRef<'_>, prefix: &str) -> Result<Self, TermError> {
        let prefix = CellValue::from_str(prefix);
        let start = IndexTerm::from_cell_name(name, prefix.as_view())?.into_bytes();
        Ok(Self {
            upper: past(start.clone()),
            lower: Bound::Included(start),
        })
    }

    /// The key bounds, usable as a `RangeBounds<[u8]>`.
    pub fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        (
            self.lower.as_ref().map(Vec::as_slice),
            self.upper.as_ref().map(Vec::as_slice),
        )
    }

    /// The key bounds, owned, as a storage scan takes them.
    pub fn into_bounds(self) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
        (self.lower, self.upper)
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

    use golemdb_cells::Width;

    use super::*;

    /// Raw, so tests can also pass names the grammar rejects.
    fn between(
        name: &[u8],
        ty: CellType,
        lower: Bound<CellValue>,
        upper: Bound<CellValue>,
    ) -> Result<Option<TermRange>, RangeError> {
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
        between(name.as_bytes(), ty, lower.cloned(), upper.cloned())
            .expect("valid range")
            .expect("non-empty range")
    }

    fn term(name: &str, value: &CellValue) -> IndexTerm {
        IndexTerm::from_str_name(name, value.as_view()).expect("valid term")
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
        let range = range("a", U32, Included(&u32(1)), Included(&u32(7)));
        assert_eq!(range.bounds(), (Included(ONE), Included(SEVEN)));
    }

    #[test]
    fn excluded_bounds_are_the_terms_of_their_values() {
        let range = range("a", U32, Excluded(&u32(1)), Excluded(&u32(7)));
        assert_eq!(range.bounds(), (Excluded(ONE), Excluded(SEVEN)));
    }

    #[test]
    fn an_unbounded_lower_side_starts_at_the_run() {
        let range = range("a", U32, Unbounded, Included(&u32(7)));
        assert_eq!(range.bounds(), (Included(RUN_START), Included(SEVEN)));
    }

    #[test]
    fn an_unbounded_upper_side_ends_past_the_run() {
        let range = range("a", U32, Included(&u32(1)), Unbounded);
        assert_eq!(range.bounds(), (Included(ONE), Excluded(RUN_END)));
    }

    #[test]
    fn two_unbounded_sides_are_the_whole_run() {
        let range = range("a", U32, Unbounded, Unbounded);
        assert_eq!(range.bounds(), (Included(RUN_START), Excluded(RUN_END)));
    }

    #[test]
    fn a_bound_of_another_type_is_a_mismatch() {
        let result = between(b"a", U32, Unbounded, Excluded(CellValue::from_i32(1)));
        let mismatch = RangeError::TypeMismatch {
            expected: U32,
            actual: CellType::Int(Width::W4),
        };
        assert_eq!(result, Err(mismatch));
    }

    #[test]
    fn inverted_bounds_are_empty() {
        for (lower, upper) in [
            (Included(u32(7)), Included(u32(1))),
            (Excluded(u32(7)), Excluded(u32(1))),
            (Included(u32(1)), Excluded(u32(0))),
        ] {
            let result = between(b"a", U32, lower.clone(), upper.clone());
            assert_eq!(result, Ok(None), "{lower:?}..{upper:?}");
        }
    }

    #[test]
    fn equal_bounds_are_empty_unless_both_are_included() {
        let (run, neighbours) = neighbourhood();
        let all = [run.clone(), neighbours].concat();
        let seven = u32(7);
        assert_eq!(
            within(&range("a", U32, Included(&seven), Included(&seven)), &all),
            [&run[2]]
        );
        for (lower, upper) in [
            (Included(u32(7)), Excluded(u32(7))),
            (Excluded(u32(7)), Included(u32(7))),
            (Excluded(u32(7)), Excluded(u32(7))),
        ] {
            let result = between(b"a", U32, lower.clone(), upper.clone());
            assert_eq!(result, Ok(None), "{lower:?}..{upper:?}");
        }
    }

    #[test]
    fn a_name_that_is_not_utf8_is_invalid() {
        let result = between(&[0xff], U32, Included(u32(1)), Unbounded);
        assert_eq!(result, Err(RangeError::Term(TermError::InvalidName)));
    }

    #[test]
    fn a_name_outside_the_grammar_is_invalid() {
        let result = between(b"1a", U32, Unbounded, Included(u32(1)));
        assert_eq!(result, Err(RangeError::Term(TermError::InvalidName)));
    }

    #[test]
    fn bytes_has_no_run() {
        let unsupported = Err(RangeError::Term(TermError::UnsupportedType(
            CellType::Bytes,
        )));
        let bytes = Included(CellValue::from_bytes(b"x"));
        assert_eq!(
            between(b"a", CellType::Bytes, bytes, Unbounded),
            unsupported
        );
        assert_eq!(
            between(b"a", CellType::Bytes, Unbounded, Unbounded),
            unsupported
        );
    }

    #[test]
    fn the_types_are_checked_before_the_name_and_emptiness_after() {
        assert_eq!(
            between(b"1a", U32, Unbounded, Unbounded),
            Err(RangeError::Term(TermError::InvalidName))
        );
        assert!(matches!(
            between(
                b"1a",
                U32,
                Included(u32(1)),
                Included(CellValue::from_u64(1))
            ),
            Err(RangeError::TypeMismatch { .. })
        ));
        assert_eq!(
            between(b"1a", U32, Included(u32(7)), Included(u32(1))),
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
            range("a", U32, Unbounded, Included(&max)),
            range("a", U32, Included(&min), Unbounded),
            range("a", U32, Unbounded, Unbounded),
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
            let got = within(&range("a", U32, lower, upper), &all);
            assert!(got.into_iter().eq(expected), "{lower:?}..{upper:?}");
        }
    }

    #[test]
    fn the_empty_string_is_the_least_str() {
        let (empty, a) = (CellValue::from_str(""), CellValue::from_str("a"));
        let all = [term("s", &empty), term("s", &a)];
        let at_most_empty = range("s", CellType::Str, Unbounded, Included(&empty));
        assert!(within(&at_most_empty, &all).into_iter().eq(&all[..1]));
        // The excluded empty string is the run's own prefix: nothing lies below.
        let below_empty = between(b"s", CellType::Str, Unbounded, Excluded(empty.clone()));
        assert_eq!(below_empty, Ok(None));
        let above_empty = range("s", CellType::Str, Excluded(&empty), Unbounded);
        assert!(within(&above_empty, &all).into_iter().eq(&all[1..]));
    }

    #[test]
    fn past_skips_trailing_0xff_bytes() {
        assert_eq!(past(vec![1, 2]), Bound::Excluded(vec![1, 3]));
        assert_eq!(past(vec![1, 0xff, 0xff]), Bound::Excluded(vec![2]));
        assert_eq!(past(vec![0xff, 0xff]), Bound::Unbounded);
        assert_eq!(past(vec![]), Bound::Unbounded);
    }

    /// Raw, so tests can also pass names the grammar rejects.
    fn with_prefix(name: &[u8], prefix: &str) -> Result<TermRange, TermError> {
        TermRange::with_prefix(CellNameRef::raw(name), prefix)
    }

    fn str(value: &str) -> CellValue {
        CellValue::from_str(value)
    }

    // Keys of the `str` run of `a`: name, 0x00, type tag 0x82, then UTF-8.
    const STR_RUN_START: &[u8] = &[b'a', 0, 0x82];
    const AB: &[u8] = &[b'a', 0, 0x82, b'a', b'b'];
    /// `AB` with its last byte incremented: past every string starting with `ab`.
    const AC: &[u8] = &[b'a', 0, 0x82, b'a', b'c'];
    /// The run's prefix with the type tag incremented: past every `str` of `a`.
    const STR_RUN_END: &[u8] = &[b'a', 0, 0x83];

    #[test]
    fn a_prefix_starts_at_its_term_and_ends_past_it() {
        let range = with_prefix(b"a", "ab").expect("valid prefix");
        assert_eq!(range.bounds(), (Included(AB), Excluded(AC)));
    }

    #[test]
    fn the_empty_prefix_is_the_whole_run() {
        let range = with_prefix(b"a", "").expect("valid prefix");
        assert_eq!(
            range.bounds(),
            (Included(STR_RUN_START), Excluded(STR_RUN_END))
        );
    }

    #[test]
    fn a_prefix_ending_in_the_greatest_char_ends_past_its_last_byte() {
        // U+10FFFF is F4 8F BF BF. UTF-8 has no 0xff byte, so the last byte
        // is always incremented.
        let range = with_prefix(b"a", "\u{10FFFF}").expect("valid prefix");
        let start: &[u8] = &[b'a', 0, 0x82, 0xf4, 0x8f, 0xbf, 0xbf];
        let end: &[u8] = &[b'a', 0, 0x82, 0xf4, 0x8f, 0xbf, 0xc0];
        assert_eq!(range.bounds(), (Included(start), Excluded(end)));
    }

    #[test]
    fn a_prefix_holds_exactly_the_strings_that_start_with_it() {
        let strings = [
            "",
            "a",
            "aB",
            "ab",
            "abc",
            "ab\u{10FFFF}",
            "ac",
            "b",
            "e",
            "é",
            "éa",
            "\u{10FFFF}",
            "\u{10FFFF}a",
        ];
        // Other types under `a`, some with the same leading bytes, and names
        // that `a` is a prefix of or that sort just after it.
        let neighbours = [
            term("a", &CellValue::from_bool(true)),
            term("a", &CellValue::from_bytes4(*b"abcd")),
            term("a", &u32(0)),
            term("ab", &str("ab")),
            term("a0", &str("ab")),
            term("b", &str("ab")),
            term("A", &str("ab")),
        ];
        let run = strings.map(|value| term("a", &str(value)));
        let all = [&run[..], &neighbours].concat();
        for prefix in ["", "a", "ab", "abd", "é", "e", "\u{10FFFF}"] {
            let range = with_prefix(b"a", prefix).expect("valid prefix");
            let mut expected: Vec<_> = strings
                .iter()
                .filter(|value| value.starts_with(prefix))
                .map(|value| term("a", &str(value)))
                .collect();
            expected.sort();
            assert!(within(&range, &all).into_iter().eq(&expected), "{prefix:?}");
        }
    }

    #[test]
    fn a_prefix_name_that_is_not_utf8_is_invalid() {
        let result = with_prefix(&[0xff], "a");
        assert_eq!(result, Err(TermError::InvalidName));
    }

    #[test]
    fn a_prefix_name_outside_the_grammar_is_invalid() {
        let result = with_prefix(b"1a", "a");
        assert_eq!(result, Err(TermError::InvalidName));
    }
}
