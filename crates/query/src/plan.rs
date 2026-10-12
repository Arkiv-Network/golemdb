//! [`Plan`]: a filter after admission. Building it validates the whole filter
//! before any state read and resolves its leaves to the index work they need.
//! Pure; no storage access.

use golemdb_cells::{CellName, CellValue};
use golemdb_index::{IndexTerm, TermError};

use crate::{Filter, QueryError, Result};

/// The written filter with its terms resolved: same shape, same order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// One term read: the postings of one exact term.
    TermRead(IndexTerm),
}

impl TryFrom<&Filter> for Plan {
    type Error = QueryError;

    /// Admission: fails with [`QueryError::InvalidArgument`] on a leaf with no
    /// valid index term.
    fn try_from(filter: &Filter) -> Result<Self> {
        match filter {
            Filter::Eq { field, value } => Ok(Self::TermRead(term(field, value)?)),
        }
    }
}

/// The term `field = value` is indexed under. The name is decoded as the
/// write path decodes it, and the value's stored bytes are the term's, so
/// queries and writes share one encoding.
fn term(field: &CellName, value: &CellValue) -> Result<IndexTerm> {
    std::str::from_utf8(field.as_bytes())
        .map_err(|_| TermError::InvalidName)
        .and_then(|name| IndexTerm::for_value(name, value.as_view()))
        .map_err(|source| QueryError::InvalidArgument {
            field: field.clone(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{CellNameRef, CellType};

    use super::*;

    #[test]
    fn valid_leaf_resolves_to_its_index_term() {
        let field: CellName = CellNameRef::raw(b"price").into();
        let value = CellValue::from_u32(7);
        let expected = IndexTerm::for_value("price", value.as_view()).expect("valid term");
        assert_eq!(term(&field, &value).expect("valid leaf"), expected);
    }

    #[test]
    fn non_utf8_name_is_invalid_name() {
        let field: CellName = CellNameRef::raw(&[0xff]).into();
        let result = term(&field, &CellValue::from_u32(7));
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument {
                source: TermError::InvalidName,
                ..
            })
        ));
    }

    #[test]
    fn term_errors_are_kept_as_the_cause() {
        let bad_name: CellName = CellNameRef::raw(b"1bad").into();
        let result = term(&bad_name, &CellValue::from_u32(7));
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument {
                source: TermError::InvalidName,
                ..
            })
        ));

        let field: CellName = CellNameRef::raw(b"price").into();
        let result = term(&field, &CellValue::from_bytes(b"x"));
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument {
                source: TermError::UnsupportedType(CellType::Bytes),
                ..
            })
        ));
    }

    #[test]
    fn invalid_argument_names_the_field() {
        let field: CellName = CellNameRef::raw(b"1bad").into();
        let result = term(&field, &CellValue::from_u32(7));
        assert!(matches!(
            result,
            Err(QueryError::InvalidArgument { field: got, .. }) if got == field
        ));
    }
}
