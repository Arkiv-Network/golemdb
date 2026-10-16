//! [`InValues`]: the values of an `In` leaf, of one indexable type.

use golemdb_cells::CellValue;

use super::values::{ValuesError, into_field, of_indexable_type};
use golemdb_cells::marker::Indexable;

/// The values of an [`In`](super::Filter::In): distinct values of one
/// indexable cell type, ascending in that type's order, which is also the
/// order of their index terms.
///
/// The field is private, so every `InValues` is valid however it was built:
///
/// - [`InValues::of`] takes a marker from `golemdb_cells::marker` and plain
///   Rust values. The marker fixes the cell type at compile time, so a
///   second type, or variable-length `bytes`, does not compile, and building
///   cannot fail.
/// - `TryFrom<Vec<CellValue>>` is the runtime path, for values that only
///   exist as `CellValue`s (a parsed query string, a deserialized request).
///   It checks at run time what the marker guarantees.
///
/// Both sort and deduplicate, and keep every value as a field: `In [b, a, b]`
/// is `In [a, b]`, equal and read in the same order. No value matches
/// nothing; an empty `InValues` has no type, so every empty one is equal.
///
/// ```
/// use golemdb_cells::marker::{Dec32, Str, U32};
/// use golemdb_query::InValues;
///
/// let prices = InValues::of(Dec32, [123_450, 99_900]);
/// assert_eq!(InValues::of(Str, ["red", "blue", "red"]), InValues::of(Str, ["blue", "red"]));
/// assert!(InValues::of(U32, []).as_slice().is_empty());
/// # let _ = prices;
/// ```
///
/// A second type does not compile:
///
/// ```compile_fail
/// use golemdb_cells::marker::U32;
/// use golemdb_query::InValues;
///
/// InValues::of(U32, [1_u32, 2_u64]);
/// ```
///
/// Nor does variable-length `bytes`, whose marker is not `Indexable`:
///
/// ```compile_fail
/// use golemdb_cells::marker::Bytes;
/// use golemdb_query::InValues;
///
/// InValues::of(Bytes, [b"any length".as_slice()]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InValues {
    /// Fields of one indexable type, strictly ascending by payload.
    values: Vec<CellValue>,
}

impl InValues {
    /// `values`, as cells of the type `ty` names, each built as that type's
    /// `CellValue::from_*` constructor builds it.
    pub fn of<'a, T: Indexable>(ty: T, values: impl IntoIterator<Item = T::Value<'a>>) -> Self {
        let _ = ty; // A value only to name `T` without a turbofish.
        Self::canonical(values.into_iter().map(T::cell).collect())
    }

    /// The values: distinct, ascending, all fields of one type.
    pub fn as_slice(&self) -> &[CellValue] {
        &self.values
    }

    /// Ascending and distinct. All values are fields of one type, so payload
    /// order is the type's order (the cells' order encoding) and term order.
    fn canonical(values: Vec<CellValue>) -> Self {
        let values = {
            let mut values = values;
            values.sort_unstable_by(|a, b| a.value().cmp(b.value()));
            values.dedup();
            values
        };
        Self { values }
    }
}

impl TryFrom<Vec<CellValue>> for InValues {
    type Error = ValuesError;

    /// The runtime path: values of one type, the first value's, and that
    /// type indexable. No value is valid.
    fn try_from(values: Vec<CellValue>) -> Result<Self, Self::Error> {
        if let [first, ..] = values.as_slice() {
            of_indexable_type(first.cell_type(), &values)?;
        }
        Ok(Self::canonical(
            values.into_iter().map(into_field).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{
        CellKind, CellType, FloatWidth, NotNan64, Width,
        marker::{Bytes20, Date32, Dec32, F64, I32, I256, Str, Timestamp64, U32},
    };

    use super::*;

    /// The values' one type, `None` when there is no value.
    fn cell_type(values: &InValues) -> Option<CellType> {
        values.as_slice().first().map(CellValue::cell_type)
    }

    #[test]
    fn the_marker_names_the_type_of_every_value() {
        for (values, ty) in [
            (InValues::of(U32, [3, 1]), CellType::Uint(Width::W4)),
            (InValues::of(I32, [3, 1]), CellType::Int(Width::W4)),
            (InValues::of(Dec32, [3, 1]), CellType::Decimal(Width::W4)),
            (InValues::of(Date32, [3, 1]), CellType::Date32),
            (InValues::of(Timestamp64, [3, 1]), CellType::Timestamp64),
            (
                InValues::of(I256, [[0xff; 32], [0; 32]]),
                CellType::Int(Width::W32),
            ),
            (InValues::of(Bytes20, [[1; 20]]), CellType::Bytes20),
            (InValues::of(Str, ["a", "b"]), CellType::Str),
        ] {
            assert!(
                values
                    .as_slice()
                    .iter()
                    .all(|value| value.cell_type() == ty),
                "{values:?}"
            );
            assert_eq!(cell_type(&values), Some(ty));
        }
        // The same Rust values under different markers are different leaves.
        assert_ne!(InValues::of(I32, [1]), InValues::of(Dec32, [1]));
        assert_ne!(InValues::of(I32, [1]), InValues::of(Date32, [1]));
    }

    #[test]
    fn values_are_ascending_in_the_types_order_and_distinct() {
        // The type's order, not the written or the Rust bytes order: -1
        // before 1 once the sign bit is flipped, "ab" before "b", -1 before
        // 0 for i256, -inf before 0.0, and -0.0 is 0.0.
        let got = InValues::of(I32, [1, -1, 1, i32::MIN]);
        assert_eq!(got.as_slice(), [i32::MIN, -1, 1].map(CellValue::from_i32));
        let got = InValues::of(Str, ["b", "ab", "b", ""]);
        assert_eq!(got.as_slice(), ["", "ab", "b"].map(CellValue::from_str));
        let got = InValues::of(I256, [[0; 32], [0xff; 32]]);
        assert_eq!(
            got.as_slice(),
            [[0xff; 32], [0; 32]].map(CellValue::from_i256_be)
        );
        let f64 = |value| NotNan64::new(value).expect("not NaN");
        let got = InValues::of(F64, [f64(0.0), f64(f64::NEG_INFINITY), f64(-0.0)]);
        let expected =
            [f64::NEG_INFINITY, 0.0].map(|value| CellValue::from_f64(value).expect("not NaN"));
        assert_eq!(got.as_slice(), expected);
        assert_eq!(cell_type(&got), Some(CellType::Float(FloatWidth::F64)));
    }

    #[test]
    fn equal_sets_are_equal_values() {
        assert_eq!(InValues::of(U32, [2, 1, 2]), InValues::of(U32, [1, 2]));
        // No value is no value, whatever the marker: both match nothing.
        assert_eq!(InValues::of(U32, []), InValues::of(Str, []));
        assert_eq!(cell_type(&InValues::of(U32, [])), None);
    }

    #[test]
    fn the_runtime_path_builds_what_the_marker_builds() {
        let runtime = InValues::try_from(vec![
            CellValue::from_dec32_unscaled(2),
            CellValue::from_dec32_unscaled(1)
                .with_kind(CellKind::Attribute)
                .expect("indexable"),
            CellValue::from_dec32_unscaled(2),
        ]);
        assert_eq!(runtime, Ok(InValues::of(Dec32, [1, 2])));
        assert_eq!(InValues::try_from(vec![]), Ok(InValues::of(U32, [])));
    }

    #[test]
    fn the_runtime_path_rejects_two_types() {
        for (values, expected, found) in [
            (
                vec![CellValue::from_u32(1), CellValue::from_u64(1)],
                CellType::Uint(Width::W4),
                CellType::Uint(Width::W8),
            ),
            (
                vec![
                    CellValue::from_i32(1),
                    CellValue::from_i32(2),
                    CellValue::from_dec32_unscaled(3),
                ],
                CellType::Int(Width::W4),
                CellType::Decimal(Width::W4),
            ),
            // Two types are reported before `bytes`, as admission does today.
            (
                vec![CellValue::from_str("x"), CellValue::from_bytes(b"x")],
                CellType::Str,
                CellType::Bytes,
            ),
        ] {
            assert_eq!(
                InValues::try_from(values.clone()),
                Err(ValuesError::MixedTypes { expected, found }),
                "{values:?}"
            );
        }
    }

    #[test]
    fn the_runtime_path_rejects_variable_length_bytes() {
        for values in [
            vec![CellValue::from_bytes(b"x")],
            vec![CellValue::from_bytes(b"x"), CellValue::from_bytes(b"")],
        ] {
            assert_eq!(
                InValues::try_from(values.clone()),
                Err(ValuesError::NotIndexable),
                "{values:?}"
            );
        }
    }
}
