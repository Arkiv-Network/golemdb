//! [`SortKey`]: one term of a query's ordering, of an indexable type.

use golemdb_cells::{CellName, CellNameRef, CellType, marker::Indexable};

use super::values::{ValuesError, indexable};

/// Order matched records by `field`, read as one indexable cell type. The
/// fields are private, as [`ExistsType`](super::ExistsType)'s is, so every
/// `SortKey` is of an indexable type however it was built:
///
/// - [`SortKey::asc`] and [`SortKey::desc`] take a marker from
///   `golemdb_cells::marker`. The marker names the type at compile time, so
///   variable-length `bytes` does not compile, and building cannot fail.
/// - [`SortKey::try_new`] is the runtime path, for a type only known at run
///   time. It checks at run time what the marker guarantees.
///
/// The name is taken raw and validated when the query runs. A record's cell
/// of that type sorts whatever its kind, field or attribute; a cell of
/// another type under the same name is absent. Absent values sort below
/// every present value: first ascending, last descending. Records that tie
/// come in ascending record ID order, in either direction.
///
/// ```
/// use golemdb_cells::{CellType, Width, marker::{Str, U64}};
/// use golemdb_query::{Direction, SortKey};
///
/// let cheapest = SortKey::asc("price", U64);
/// let u64 = CellType::Uint(Width::W8);
/// assert_eq!(SortKey::try_new("price", u64, Direction::Asc), Ok(cheapest));
/// let by_name = SortKey::desc("name", Str);
/// # let _ = by_name;
/// ```
///
/// Variable-length `bytes`, whose marker is not `Indexable`, does not
/// compile:
///
/// ```compile_fail,E0277
/// use golemdb_cells::marker::Bytes;
/// use golemdb_query::SortKey;
///
/// SortKey::asc("blob", Bytes);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    field: CellName,
    /// An indexable type.
    ty: CellType,
    direction: Direction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
}

impl SortKey {
    /// Ascending by `field`, read as the type the marker names.
    pub fn asc<T: Indexable>(field: impl AsRef<[u8]>, _type: T) -> Self {
        Self::new(field, T::CELL_TYPE, Direction::Asc)
    }

    /// Descending by `field`, read as the type the marker names.
    pub fn desc<T: Indexable>(field: impl AsRef<[u8]>, _type: T) -> Self {
        Self::new(field, T::CELL_TYPE, Direction::Desc)
    }

    /// By `field`, read as `ty`, for a type only known at run time.
    ///
    /// # Errors
    ///
    /// [`ValuesError::NotIndexable`] when `ty` is variable-length `bytes`.
    pub fn try_new(
        field: impl AsRef<[u8]>,
        ty: CellType,
        direction: Direction,
    ) -> Result<Self, ValuesError> {
        indexable(ty)?;
        Ok(Self::new(field, ty, direction))
    }

    /// The name of the cell records are ordered by.
    pub fn field(&self) -> &CellName {
        &self.field
    }

    /// The type that cell is read as.
    pub fn cell_type(&self) -> CellType {
        self.ty
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    fn new(field: impl AsRef<[u8]>, ty: CellType, direction: Direction) -> Self {
        Self {
            field: CellNameRef::raw(field.as_ref()).into(),
            ty,
            direction,
        }
    }
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{
        Width,
        marker::{Date32, Str, U64},
    };

    use super::*;

    #[test]
    fn the_marker_names_the_type_and_the_step_the_direction() {
        let price: CellName = CellNameRef::raw(b"price").into();
        let ascending = SortKey::asc("price", U64);
        assert_eq!(ascending.field(), &price);
        assert_eq!(ascending.cell_type(), CellType::Uint(Width::W8));
        assert_eq!(ascending.direction(), Direction::Asc);
        let descending = SortKey::desc("price", Str);
        assert_eq!(descending.field(), &price);
        assert_eq!(descending.cell_type(), CellType::Str);
        assert_eq!(descending.direction(), Direction::Desc);
    }

    #[test]
    fn the_name_is_taken_raw() {
        let raw: CellName = CellNameRef::raw(b"1 not valid").into();
        assert_eq!(SortKey::asc("1 not valid", U64).field(), &raw);
    }

    #[test]
    fn the_runtime_path_builds_what_the_marker_builds() {
        assert_eq!(
            SortKey::try_new("date", CellType::Date32, Direction::Desc),
            Ok(SortKey::desc("date", Date32))
        );
        assert_eq!(
            SortKey::try_new("price", CellType::Uint(Width::W8), Direction::Asc),
            Ok(SortKey::asc("price", U64))
        );
    }

    #[test]
    fn the_runtime_path_keeps_every_indexable_type() {
        let indexable: Vec<_> = (0..128)
            .filter_map(|id| CellType::from_id(id).ok())
            .filter(|ty| ty.can_be_attribute())
            .collect();
        assert!(!indexable.is_empty());
        for ty in indexable {
            let key = SortKey::try_new("f", ty, Direction::Asc).expect("indexable");
            assert_eq!(key.cell_type(), ty);
        }
    }

    #[test]
    fn the_runtime_path_rejects_variable_length_bytes() {
        for direction in [Direction::Asc, Direction::Desc] {
            assert_eq!(
                SortKey::try_new("blob", CellType::Bytes, direction),
                Err(ValuesError::NotIndexable)
            );
        }
    }
}
