use golemdb_cells::CellChange;
use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{Database, ReadCursor, WriteTransaction};

use crate::{
    BranchError, Result,
    buffer::Buffered,
    commit::{replay, write_atomically},
    gc,
    head::{Head, write_format, write_head},
    tables,
};

/// Create commit zero in a caller-owned writer, alongside the caller's rows.
///
/// Engine tables must be empty. The genesis cells, including reserved records,
/// allocation and parameters, are the caller's policy; this builds their
/// cell/index commitments, node reference counts, format rows and head. The
/// caller may then add its own `Superblock` identifiers in the same writer.
/// It must commit on success and abort on error or panic.
pub fn create_genesis_in(
    tx: &mut impl WriteTransaction,
    hasher: &impl HashProvider,
    cells: impl IntoIterator<Item = CellChange>,
) -> Result<Head> {
    for table in tables::ENGINE {
        if tx.cursor(table, b"")?.next()?.is_some() {
            return Err(BranchError::DatabaseNotEmpty(table));
        }
    }
    let mut buffer = Buffered::new(&*tx);
    let (cells, index) =
        crate::state::apply(&mut buffer, hasher, RootRef::Empty, RootRef::Empty, cells)?;
    gc::update(
        &mut buffer,
        gc::RETENTION,
        RootRef::Empty,
        RootRef::Empty,
        &cells,
        &index,
    )?;
    write_format(&mut buffer, gc::RETENTION)?;
    let head = Head {
        commit_id: 0,
        state_root: cells.root.hash(hasher),
        index_root: index.root.hash(hasher),
    };
    write_head(&mut buffer, &head)?;
    let writes = buffer.into_writes();
    replay(tx, &writes)?;
    Ok(head)
}

/// [`create_genesis_in`] in its own committed writer.
pub(crate) fn create(
    database: &impl Database,
    hasher: &impl HashProvider,
    cells: impl IntoIterator<Item = CellChange>,
) -> Result<()> {
    write_atomically(database.begin_write()?, |tx| {
        create_genesis_in(tx, hasher, cells)?;
        Ok(())
    })
}
