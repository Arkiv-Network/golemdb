//! [`SortKey`]: one term of a query's ordering.

use golemdb_cells::{CellName, CellNameRef, CellType};

/// Order matched records by `field`, read as `ty`. A cell of another type
/// under the same name is absent; absent values sort last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    pub field: CellName,
    pub ty: CellType,
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
}

impl SortKey {
    /// Never fails: the name is taken raw and validated when the query runs.
    pub fn asc(field: impl AsRef<[u8]>, ty: CellType) -> Self {
        Self::new(field, ty, Direction::Asc)
    }

    /// Never fails: the name is taken raw and validated when the query runs.
    pub fn desc(field: impl AsRef<[u8]>, ty: CellType) -> Self {
        Self::new(field, ty, Direction::Desc)
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
    use golemdb_cells::Width;

    use super::*;

    #[test]
    fn asc_and_desc_set_the_direction() {
        let price: CellName = CellNameRef::raw(b"price").into();
        let ty = CellType::Uint(Width::W8);
        assert_eq!(
            SortKey::asc("price", ty),
            SortKey {
                field: price.clone(),
                ty,
                direction: Direction::Asc
            }
        );
        assert_eq!(
            SortKey::desc("price", ty),
            SortKey {
                field: price,
                ty,
                direction: Direction::Desc
            }
        );
    }
}
