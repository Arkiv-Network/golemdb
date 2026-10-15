use crate::buffer::Buffered;
use golemdb_storage::{
    MemoryStore, ReadCursor, ReadTransaction, StorageError, Store, Table, WriteTransaction,
};
use proptest::prelude::*;

proptest! {
    // Compare all cursor direction changes and boundaries with a real storage
    // writer holding the same logical rows; the implementation uses no snapshot
    // copying and may have to skip arbitrarily many tombstones.
    #[test]
    fn buffer_matches_storage_for_writes_and_bidirectional_cursors(
        initial in prop::collection::vec((0u8..12, any::<u8>()), 0..25),
        changes in prop::collection::vec((0u8..3, 0u8..12, any::<u8>()), 0..40),
        directions in prop::collection::vec(any::<bool>(), 0..60),
    ) {
        let table = Table("test");
        let db = MemoryStore::new();
        let mut seed = db.begin_write().unwrap();
        for (key, value) in initial { seed.put(table, &[key], &[value]).unwrap(); }
        seed.commit().unwrap();
        let origin = db.begin_read().unwrap();
        let mut buffer = Buffered::new(&origin);
        let mut expected = db.begin_write().unwrap();
        for (op, key, value) in changes {
            match op {
                0 => { buffer.put(table, &[key], &[value]).unwrap(); expected.put(table, &[key], &[value]).unwrap(); }
                1 => prop_assert_eq!(buffer.delete(table, &[key]).unwrap(), expected.delete(table, &[key]).unwrap()),
                _ => {
                    let a = buffer.insert(table, &[key], &[value]);
                    let b = expected.insert(table, &[key], &[value]);
                    prop_assert_eq!(a.is_ok(), b.is_ok());
                    if a.is_err() { prop_assert!(matches!(a, Err(StorageError::AlreadyExists))); }
                }
            }
        }
        for start in 0..14 {
            prop_assert_eq!(buffer.get(table, &[start]).unwrap(), expected.get(table, &[start]).unwrap());
            let mut reverse_a = buffer.cursor(table, &[start]).unwrap();
            let mut reverse_b = expected.cursor(table, &[start]).unwrap();
            prop_assert_eq!(reverse_a.prev().unwrap(), reverse_b.prev().unwrap());
            prop_assert_eq!(reverse_a.next().unwrap(), reverse_b.next().unwrap());
            let mut a = buffer.cursor(table, &[start]).unwrap();
            let mut b = expected.cursor(table, &[start]).unwrap();
            // Force both endpoints as well as random reversal sequences.
            for forward in std::iter::repeat_n(true, 16).chain(std::iter::repeat_n(false, 16)).chain(directions.iter().copied()) {
                let actual = if forward { a.next() } else { a.prev() }.unwrap();
                let wanted = if forward { b.next() } else { b.prev() }.unwrap();
                prop_assert_eq!(actual, wanted);
            }
        }
        let mut a = buffer.cursor(Table("absent"), b"").unwrap();
        prop_assert_eq!(a.next().unwrap(), None);
        prop_assert_eq!(a.prev().unwrap(), None);
    }
}

#[test]
fn buffer_rejects_invalid_tables_and_cannot_publish() {
    let db = MemoryStore::new();
    let origin = db.begin_read().unwrap();
    let mut tx = Buffered::new(&origin);
    assert!(matches!(
        tx.put(Table(""), b"a", b"b"),
        Err(StorageError::InvalidTableName(_))
    ));
    tx.put(Table("test"), b"a", b"b").unwrap();
    assert!(tx.commit().is_err());
    assert_eq!(
        db.begin_read().unwrap().get(Table("test"), b"a").unwrap(),
        None
    );
}
