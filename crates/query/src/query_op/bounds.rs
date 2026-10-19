//! [`Bounds`]: the bounds of a `Range` leaf, of one indexable type.

use std::ops::Bound;

use golemdb_cells::{CellType, CellValue, marker::Indexable};

use super::values::{ValuesError, into_field, of_indexable_type};

/// The bounds of a [`Range`](super::Filter::Range): an indexable cell type,
/// and both bounds of that type, as fields. The fields are private, as
/// [`InValues`](super::InValues)' are, so every `Bounds` is valid however it
/// was built:
///
/// - [`Bounds::new`] takes a marker from `golemdb_cells::marker`, which types
///   both sides at compile time;
/// - [`Bounds::try_new`] is the runtime path, for a type and values only
///   known at run time.
///
/// The type is kept even when no side holds a value: two `Unbounded` sides
/// match every cell of that type. Bounds that admit no value (inverted, or
/// equal with a side excluded) are valid and match nothing.
///
/// ```
/// use std::ops::Bound::{Excluded, Included, Unbounded};
///
/// use golemdb_cells::marker::{Str, Timestamp64};
/// use golemdb_query::Bounds;
///
/// let names = Bounds::new(Str, Included("a"), Excluded("b"));
/// let since = Bounds::new(Timestamp64, Included(1_700_000_000_000_000), Unbounded);
/// let any_name = Bounds::new(Str, Unbounded, Unbounded);
/// # let _ = (names, since, any_name);
/// ```
///
/// Bounds of two types do not compile:
///
/// ```compile_fail,E0308
/// use std::ops::Bound::{Excluded, Included};
///
/// use golemdb_cells::marker::U32;
/// use golemdb_query::Bounds;
///
/// Bounds::new(U32, Included(1_u32), Excluded(10_u64));
/// ```
///
/// Nor do bounds of variable-length `bytes`:
///
/// ```compile_fail,E0277
/// use std::ops::Bound::{Included, Unbounded};
///
/// use golemdb_cells::marker::Bytes;
/// use golemdb_query::Bounds;
///
/// Bounds::new(Bytes, Included(b"x".as_slice()), Unbounded);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bounds {
    ty: CellType,
    lower: Bound<CellValue>,
    upper: Bound<CellValue>,
}

impl Bounds {
    /// Bounds of the cell type the marker names: one marker for both sides.
    pub fn new<'a, T: Indexable>(
        _: T,
        lower: Bound<T::Value<'a>>,
        upper: Bound<T::Value<'a>>,
    ) -> Self {
        Self {
            ty: T::CELL_TYPE,
            lower: lower.map(T::cell),
            upper: upper.map(T::cell),
        }
    }

    /// Bounds of type `ty`, for a type and values only known at run time.
    ///
    /// # Errors
    ///
    /// [`ValuesError::MixedTypes`] for a bound of another type than `ty`,
    /// then [`ValuesError::NotIndexable`] when `ty` is variable-length
    /// `bytes`.
    pub fn try_new(
        ty: CellType,
        lower: Bound<CellValue>,
        upper: Bound<CellValue>,
    ) -> Result<Self, ValuesError> {
        of_indexable_type(ty, [&lower, &upper].into_iter().filter_map(bound_value))?;
        Ok(Self {
            ty,
            lower: lower.map(into_field),
            upper: upper.map(into_field),
        })
    }

    /// The type of the cells the range matches.
    pub fn cell_type(&self) -> CellType {
        self.ty
    }

    /// The lower bound.
    pub fn lower(&self) -> Bound<&CellValue> {
        self.lower.as_ref()
    }

    /// The upper bound.
    pub fn upper(&self) -> Bound<&CellValue> {
        self.upper.as_ref()
    }
}

/// The value of a bounded side.
fn bound_value(bound: &Bound<CellValue>) -> Option<&CellValue> {
    match bound {
        Bound::Included(value) | Bound::Excluded(value) => Some(value),
        Bound::Unbounded => None,
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Bound::{Excluded, Included, Unbounded};

    use golemdb_cells::{
        CellKind, Width,
        marker::{Str, Timestamp64, U32, U64},
    };

    use super::*;

    #[test]
    fn one_marker_types_both_bounds() {
        let bounds = Bounds::new(Str, Included("a"), Excluded("b"));
        assert_eq!(bounds.lower(), Included(&CellValue::from_str("a")));
        assert_eq!(bounds.upper(), Excluded(&CellValue::from_str("b")));
        assert_eq!(bounds.cell_type(), CellType::Str);
        let since = Bounds::new(Timestamp64, Included(1_700_000_000_000_000), Unbounded);
        assert_eq!(since.upper(), Unbounded);
    }

    #[test]
    fn two_unbounded_sides_keep_the_markers_type() {
        let any = Bounds::new(U32, Unbounded, Unbounded);
        assert_eq!(any.cell_type(), CellType::Uint(Width::W4));
        assert_ne!(any, Bounds::new(U64, Unbounded, Unbounded));
    }

    #[test]
    fn the_runtime_path_builds_what_the_marker_builds() {
        let u32 = CellType::Uint(Width::W4);
        let nine = CellValue::from_u32(9)
            .with_kind(CellKind::Attribute)
            .expect("indexable");
        let runtime = Bounds::try_new(u32, Unbounded, Included(nine));
        assert_eq!(runtime, Ok(Bounds::new(U32, Unbounded, Included(9))));
        let runtime = Bounds::try_new(u32, Unbounded, Unbounded);
        assert_eq!(runtime, Ok(Bounds::new(U32, Unbounded, Unbounded)));
    }

    #[test]
    fn the_runtime_path_rejects_a_bound_of_another_type() {
        let u32 = CellType::Uint(Width::W4);
        for (lower, upper, found) in [
            (
                Included(CellValue::from_u32(1)),
                Excluded(CellValue::from_u64(2)),
                CellType::Uint(Width::W8),
            ),
            (
                Unbounded,
                Included(CellValue::from_u64(2)),
                CellType::Uint(Width::W8),
            ),
            (
                Excluded(CellValue::from_bytes(b"x")),
                Unbounded,
                CellType::Bytes,
            ),
        ] {
            assert_eq!(
                Bounds::try_new(u32, lower.clone(), upper.clone()),
                Err(ValuesError::MixedTypes {
                    expected: u32,
                    found
                }),
                "{lower:?}..{upper:?}"
            );
        }
    }

    #[test]
    fn the_runtime_path_rejects_variable_length_bytes() {
        let bytes = Included(CellValue::from_bytes(b"x"));
        for (lower, upper) in [(bytes.clone(), Unbounded), (Unbounded, Unbounded)] {
            assert_eq!(
                Bounds::try_new(CellType::Bytes, lower, upper),
                Err(ValuesError::NotIndexable)
            );
        }
    }
}
