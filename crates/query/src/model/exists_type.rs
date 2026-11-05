//! [`ExistsType`]: the cell type of an `Exists` leaf, an indexable one.

use golemdb_cells::{CellType, marker::Indexable};

use super::values::{ValuesError, indexable};

/// The cell type an [`Exists`](super::Filter::Exists) asks for: an indexable
/// one. The field is private, as [`EqValue`](super::EqValue)'s is, so every
/// `ExistsType` is valid however it was built:
///
/// - [`ExistsType::of`] takes a marker from `golemdb_cells::marker`. The
///   marker names the type at compile time, so variable-length `bytes` does
///   not compile, and building cannot fail.
/// - `TryFrom<CellType>` is the runtime path, for a type only known at run
///   time. It checks at run time what the marker guarantees.
///
/// ```
/// use golemdb_cells::{CellType, marker::{Dec32, I32, Str}};
/// use golemdb_query::ExistsType;
///
/// let text = ExistsType::of(Str);
/// assert_eq!(ExistsType::try_from(CellType::Str), Ok(text));
/// // Types whose cells share a Rust value are distinct types.
/// assert_ne!(ExistsType::of(I32), ExistsType::of(Dec32));
/// ```
///
/// Variable-length `bytes`, whose marker is not `Indexable`, does not
/// compile:
///
/// ```compile_fail,E0277
/// use golemdb_cells::marker::Bytes;
/// use golemdb_query::ExistsType;
///
/// ExistsType::of(Bytes);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistsType {
    /// An indexable type.
    ty: CellType,
}

impl ExistsType {
    /// The cell type the marker names.
    pub fn of<T: Indexable>(_: T) -> Self {
        Self { ty: T::CELL_TYPE }
    }

    /// The type of the cells the leaf matches.
    pub fn cell_type(self) -> CellType {
        self.ty
    }
}

impl TryFrom<CellType> for ExistsType {
    type Error = ValuesError;

    /// The runtime path: an indexable type.
    fn try_from(ty: CellType) -> Result<Self, Self::Error> {
        indexable(ty)?;
        Ok(Self { ty })
    }
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{
        FloatWidth, Width,
        marker::{Bytes20, Date32, Dec32, F64, I32, Str, Timestamp64, U32},
    };

    use super::*;

    #[test]
    fn the_marker_names_the_type() {
        assert_eq!(ExistsType::of(U32).cell_type(), CellType::Uint(Width::W4));
        assert_eq!(ExistsType::of(Str).cell_type(), CellType::Str);
        // Markers whose cells share a Rust value name different types.
        assert_ne!(ExistsType::of(I32), ExistsType::of(Dec32));
        assert_ne!(ExistsType::of(I32), ExistsType::of(Date32));
    }

    #[test]
    fn the_runtime_path_builds_what_the_marker_builds() {
        for (ty, typed) in [
            (CellType::Str, ExistsType::of(Str)),
            (CellType::Bytes20, ExistsType::of(Bytes20)),
            (CellType::Decimal(Width::W4), ExistsType::of(Dec32)),
            (CellType::Float(FloatWidth::F64), ExistsType::of(F64)),
            (CellType::Timestamp64, ExistsType::of(Timestamp64)),
        ] {
            assert_eq!(ExistsType::try_from(ty), Ok(typed), "{ty:?}");
        }
    }

    #[test]
    fn the_runtime_path_keeps_every_indexable_type() {
        let indexable: Vec<_> = (0..128)
            .filter_map(|id| CellType::from_id(id).ok())
            .filter(|ty| ty.can_be_attribute())
            .collect();
        assert_eq!(indexable.len(), 23);
        for ty in indexable {
            assert_eq!(ExistsType::try_from(ty).map(ExistsType::cell_type), Ok(ty));
        }
    }

    #[test]
    fn the_runtime_path_rejects_variable_length_bytes() {
        assert_eq!(
            ExistsType::try_from(CellType::Bytes),
            Err(ValuesError::NotIndexable)
        );
    }
}
