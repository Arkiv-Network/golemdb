//! Cell/index materialization shared by genesis creation and branch sealing.
use golemdb_cells::{CellChange, CellKey, CellValue, Cells, CellsUpdate};
use golemdb_index::{Index, IndexTerm, IndexUpdate, PostingChange, TermError};
use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::WriteTransaction;

use crate::Result;

/// Materialize one net cell batch and derive postings from its actual changes.
/// Transaction ownership, system cells and publication belong to the caller.
pub(crate) fn apply(
    tx: &mut impl WriteTransaction,
    hasher: &impl HashProvider,
    cells: RootRef<32>,
    index: RootRef<32>,
    changes: impl IntoIterator<Item = CellChange>,
) -> Result<(CellsUpdate, IndexUpdate)> {
    let cells = Cells::new(hasher).apply(tx, cells, changes)?;
    let mut postings = Vec::new();
    for change in &cells.changed_cells {
        let before = term(&change.key, change.before.as_ref())?;
        let after = term(&change.key, change.after.as_ref())?;
        if before == after {
            continue;
        }
        let record_id = change.key.record_id();
        if let Some(term) = before {
            postings.push(PostingChange::Remove { term, record_id });
        }
        if let Some(term) = after {
            postings.push(PostingChange::Add { term, record_id });
        }
    }
    let index = Index::new(hasher).apply(tx, index, postings)?;
    Ok((cells, index))
}

fn term(key: &CellKey, value: Option<&CellValue>) -> Result<Option<IndexTerm>> {
    let Some(value) = value.filter(|value| value.is_indexable()) else {
        return Ok(None);
    };
    // Binary reserved field names never reach UTF-8/name validation.
    let name = key.name();
    let name = std::str::from_utf8(name.as_bytes()).map_err(|_| TermError::InvalidName)?;
    Ok(IndexTerm::from_cell(name, value.as_view())?)
}
