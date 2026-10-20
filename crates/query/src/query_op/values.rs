//! What the runtime constructors of [`EqValue`](super::EqValue),
//! [`InValues`](super::InValues) and [`Bounds`](super::Bounds) check: values
//! of one indexable type.

use golemdb_cells::{CellKind, CellType, CellValue};

/// Why `CellValue`s cannot be one leaf's values: an `Eq`'s value, an `In`'s
/// values, or a `Range`'s bounds. The typed constructors cannot produce
/// either case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ValuesError {
    /// Values of two types: a leaf reads the terms of one type.
    #[error("values of two types, {expected:?} and {found:?}: a leaf reads one type")]
    MixedTypes { expected: CellType, found: CellType },
    /// Values of variable-length `bytes`, which has no index term.
    #[error("variable-length `bytes` values have no index term")]
    NotIndexable,
}

/// `Ok` when every value is of type `expected`, and `expected` is
/// indexable. Another type is reported before `bytes`.
pub(super) fn of_same_indexable_type<'a>(
    expected: CellType,
    values: impl IntoIterator<Item = &'a CellValue>,
) -> Result<(), ValuesError> {
    if let Some(found) = values
        .into_iter()
        .map(CellValue::cell_type)
        .find(|&found| found != expected)
    {
        return Err(ValuesError::MixedTypes { expected, found });
    }
    if expected.can_be_attribute() {
        Ok(())
    } else {
        Err(ValuesError::NotIndexable)
    }
}

/// `value` as a field: the kind plays no part in a lookup, so an attribute
/// and a field of one value are one value here.
pub(super) fn into_field(value: CellValue) -> CellValue {
    // Only `Attribute` can fail, on `bytes`.
    value
        .with_kind(CellKind::Field)
        .expect("any cell can be a field")
}
