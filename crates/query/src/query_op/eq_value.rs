//! [`EqValue`]: the value of an `Eq` leaf, of an indexable type.

use golemdb_cells::{CellType, CellValue, marker::Indexable};

use super::values::{ValuesError, into_field, of_same_indexable_type};

/// The value of an [`Eq`](super::Filter::Eq): one value of an indexable cell
/// type, as a field. The field is private, as [`InValues`](super::InValues)'
/// and [`Bounds`](super::Bounds)' are, so every `EqValue` is valid however it
/// was built:
///
/// - [`EqValue::of`] takes a marker from `golemdb_cells::marker` and a plain
///   Rust value. The marker fixes the cell type at compile time, so
///   variable-length `bytes` does not compile, and building cannot fail.
/// - `TryFrom<CellValue>` is the runtime path, for a value that only exists
///   as a `CellValue`. It checks at run time what the marker guarantees.
///
/// ```
/// use golemdb_cells::{CellValue, marker::{Dec32, I32, Str}};
/// use golemdb_query::EqValue;
///
/// let color = EqValue::of(Str, "blue");
/// assert_eq!(EqValue::try_from(CellValue::from_str("blue")), Ok(color));
/// // The same Rust value under two markers is two leaves.
/// assert_ne!(EqValue::of(I32, 5), EqValue::of(Dec32, 5));
/// ```
///
/// Variable-length `bytes`, whose marker is not `Indexable`, does not
/// compile:
///
/// ```compile_fail,E0277
/// use golemdb_cells::marker::Bytes;
/// use golemdb_query::EqValue;
///
/// EqValue::of(Bytes, b"any length".as_slice());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqValue {
    /// A field of an indexable type.
    value: CellValue,
}

impl EqValue {
    /// `value`, as a cell of the type the marker names, built as that type's
    /// `CellValue::from_*` constructor builds it.
    pub fn of<T: Indexable>(_: T, value: T::Value<'_>) -> Self {
        Self {
            value: T::cell(value),
        }
    }

    /// The value, as a field.
    pub fn as_cell(&self) -> &CellValue {
        &self.value
    }

    /// The type of the cells the leaf matches.
    pub fn cell_type(&self) -> CellType {
        self.value.cell_type()
    }
}

impl TryFrom<CellValue> for EqValue {
    type Error = ValuesError;

    /// The runtime path: a value of an indexable type.
    fn try_from(value: CellValue) -> Result<Self, Self::Error> {
        of_same_indexable_type(value.cell_type(), [&value])?;
        Ok(Self {
            value: into_field(value),
        })
    }
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{
        CellKind, Width,
        marker::{Date32, Dec32, I32, Str, U32},
    };

    use super::*;

    #[test]
    fn the_marker_names_the_values_type() {
        assert_eq!(EqValue::of(U32, 7).cell_type(), CellType::Uint(Width::W4));
        assert_eq!(EqValue::of(U32, 7).as_cell(), &CellValue::from_u32(7));
        assert_eq!(EqValue::of(Str, "é").as_cell(), &CellValue::from_str("é"));
        // The same Rust value under different markers is different leaves.
        assert_ne!(EqValue::of(I32, 1), EqValue::of(Dec32, 1));
        assert_ne!(EqValue::of(I32, 1), EqValue::of(Date32, 1));
    }

    #[test]
    fn the_runtime_path_builds_what_the_marker_builds() {
        let attribute = CellValue::from_dec32_unscaled(5)
            .with_kind(CellKind::Attribute)
            .expect("indexable");
        assert_eq!(EqValue::try_from(attribute), Ok(EqValue::of(Dec32, 5)));
        assert_eq!(
            EqValue::try_from(CellValue::from_str("x")),
            Ok(EqValue::of(Str, "x"))
        );
    }

    #[test]
    fn the_runtime_path_rejects_variable_length_bytes() {
        for value in [CellValue::from_bytes(b"x"), CellValue::from_bytes(b"")] {
            assert_eq!(
                EqValue::try_from(value.clone()),
                Err(ValuesError::NotIndexable),
                "{value:?}"
            );
        }
    }
}
