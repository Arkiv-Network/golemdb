use std::ops::{Bound, RangeBounds};

use crate::{IndexTerm, TermError, TermRange};
use golemdb_cells::{CellNameRef, CellType, CellValue, Width};

fn range(
    name: &str,
    ty: CellType,
    lower: Bound<&CellValue>,
    upper: Bound<&CellValue>,
) -> TermRange {
    TermRange::between(
        CellNameRef::raw(name.as_bytes()),
        ty,
        lower.map(CellValue::as_view),
        upper.map(CellValue::as_view),
    )
    .expect("valid range")
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
    let range = range("a", U32, Bound::Unbounded, Bound::Unbounded);
    assert!(within(&range, &all).into_iter().eq(&run));
}

#[test]
fn bounds_are_values_in_the_run() {
    let (run, neighbours) = neighbourhood();
    let all = [run.clone(), neighbours].concat();
    let (one, seven) = (CellValue::from_u32(1), CellValue::from_u32(7));
    for (lower, upper, expected) in [
        (Bound::Included(&one), Bound::Included(&seven), &run[1..3]),
        (Bound::Excluded(&one), Bound::Included(&seven), &run[2..3]),
        (Bound::Included(&one), Bound::Excluded(&seven), &run[1..2]),
        (Bound::Excluded(&one), Bound::Excluded(&seven), &run[..0]),
        (Bound::Included(&one), Bound::Unbounded, &run[1..]),
        (Bound::Excluded(&one), Bound::Unbounded, &run[2..]),
        (Bound::Unbounded, Bound::Included(&seven), &run[..3]),
        (Bound::Unbounded, Bound::Excluded(&seven), &run[..2]),
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
    let at_most_empty = range("s", ty, Bound::Unbounded, Bound::Included(&empty));
    assert!(within(&at_most_empty, &all).into_iter().eq(&all[..1]));
    // The excluded empty string is the run's own prefix: nothing lies below.
    let below_empty = range("s", ty, Bound::Unbounded, Bound::Excluded(&empty));
    assert!(within(&below_empty, &all).is_empty());
    let above_empty = range("s", ty, Bound::Excluded(&empty), Bound::Unbounded);
    assert!(within(&above_empty, &all).into_iter().eq(&all[1..]));
}

#[test]
fn invalid_ranges_are_term_errors() {
    let between = |name: &[u8], ty, lower: Bound<&CellValue>| {
        TermRange::between(
            CellNameRef::raw(name),
            ty,
            lower.map(CellValue::as_view),
            Bound::Unbounded,
        )
    };
    let one = CellValue::from_u32(1);
    assert_eq!(
        between(&[0xff], U32, Bound::Unbounded),
        Err(TermError::InvalidName)
    );
    assert_eq!(
        between(b"1a", U32, Bound::Unbounded),
        Err(TermError::InvalidName)
    );
    assert_eq!(
        between(b"a", CellType::Bytes, Bound::Unbounded),
        Err(TermError::UnsupportedType(CellType::Bytes))
    );
    let mismatch = TermError::TypeMismatch {
        expected: CellType::Uint(Width::W8),
        actual: U32,
    };
    assert_eq!(
        between(b"a", CellType::Uint(Width::W8), Bound::Included(&one)),
        Err(mismatch)
    );
    assert_eq!(
        between(b"a", CellType::Uint(Width::W8), Bound::Excluded(&one)),
        Err(mismatch)
    );
}
