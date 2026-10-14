use std::ops::Bound;

use golemdb_cells::{CellNameRef, CellType, CellValueRef};

use crate::{IndexTerm, TermError, term::name_str};

/// A range of index keys inside one name/type run, for an ordered scan with
/// [`crate::Index::terms_in`].
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
    use super::*;

    #[test]
    fn past_skips_trailing_0xff_bytes() {
        assert_eq!(past(vec![1, 2]), Bound::Excluded(vec![1, 3]));
        assert_eq!(past(vec![1, 0xff, 0xff]), Bound::Excluded(vec![2]));
        assert_eq!(past(vec![0xff, 0xff]), Bound::Unbounded);
        assert_eq!(past(vec![]), Bound::Unbounded);
    }
}
