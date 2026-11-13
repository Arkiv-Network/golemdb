//! The cell seam: every cell a sorting strategy reads goes through
//! [`CellSource`], so strategies are testable without a database.

use golemdb_cells::{CellKey, CellName, CellReader, CellValue};
use golemdb_storage::ReadTransaction;

/// Read access to committed cells at one snapshot. Use the snapshot the
/// query's match set was evaluated at, so every read sees the same state.
pub trait CellSource {
    /// The cell `name` of record `record_id`, whatever its type and kind;
    /// `None` when the record has no cell of that name.
    fn cell(&self, record_id: u64, name: &CellName) -> golemdb_cells::Result<Option<CellValue>>;
}

/// One point read of `Cell[record_id ‖ name]`.
impl<R: ReadTransaction> CellSource for CellReader<'_, R> {
    fn cell(&self, record_id: u64, name: &CellName) -> golemdb_cells::Result<Option<CellValue>> {
        self.get(&CellKey::new(record_id, name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{CellError, CellKind, CellNameRef, tables};
    use golemdb_storage::{MemoryStore, Store, WriteTransaction};

    use super::*;

    fn name(bytes: &[u8]) -> CellName {
        CellNameRef::raw(bytes).into()
    }

    /// A store holding `rows`, each stored as the cell's raw bytes.
    fn store(rows: &[(u64, &[u8], &[u8])]) -> MemoryStore {
        let store = MemoryStore::new();
        let mut tx = store.begin_write().expect("write");
        for &(record_id, cell_name, bytes) in rows {
            let key = CellKey::new(record_id, name(cell_name)).encode();
            tx.put(tables::CELL, &key, bytes).expect("put");
        }
        tx.commit().expect("commit");
        store
    }

    #[test]
    fn a_cell_is_read_whatever_its_kind() {
        let field = CellValue::from_u32(7);
        let attribute = CellValue::from_str("blue")
            .with_kind(CellKind::Attribute)
            .expect("indexable");
        let store = store(&[
            (64, b"price", field.encoded_bytes()),
            (64, b"color", attribute.encoded_bytes()),
        ]);
        let read = store.begin_read().expect("read");
        let cells = CellReader::new(&read);
        assert_eq!(cells.cell(64, &name(b"price")).expect("read"), Some(field));
        assert_eq!(
            cells.cell(64, &name(b"color")).expect("read"),
            Some(attribute)
        );
    }

    #[test]
    fn only_the_named_cell_of_the_named_record_is_read() {
        let value = CellValue::from_u32(7);
        let store = store(&[
            (64, b"prices", value.encoded_bytes()),
            (65, b"price", value.encoded_bytes()),
        ]);
        let read = store.begin_read().expect("read");
        let cells = CellReader::new(&read);
        // Neither a longer name nor another record's cell is this one.
        assert_eq!(cells.cell(64, &name(b"price")).expect("read"), None);
        assert_eq!(cells.cell(66, &name(b"price")).expect("read"), None);
    }

    #[test]
    fn a_stored_value_that_does_not_parse_is_an_error() {
        // A zero type tag is reserved as invalid.
        let store = store(&[(64, b"price", &[0x00])]);
        let read = store.begin_read().expect("read");
        assert!(matches!(
            CellReader::new(&read).cell(64, &name(b"price")),
            Err(CellError::Value(_))
        ));
    }
}
