use crate::{DEFAULT_MAX_MAP_SIZE, DEFAULT_PAGE_SIZE, MdbxOptions, MdbxStore};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// The page size MDBX uses for this database.
fn page_size(db: &MdbxStore) -> usize {
    db.inner.stat().unwrap().page_size() as usize
}

/// The address space MDBX reserved for this database.
fn map_size(db: &MdbxStore) -> usize {
    db.inner.info().unwrap().map_size()
}

#[test]
fn default_page_size_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    assert_eq!(page_size(&db), DEFAULT_PAGE_SIZE);
}

#[test]
fn configured_page_size_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let options = MdbxOptions {
        page_size: 16 * KIB,
        ..Default::default()
    };
    let db = MdbxStore::open_with_options(dir.path(), options).unwrap();
    assert_eq!(page_size(&db), 16 * KIB);
}

#[test]
fn reopened_database_keeps_its_page_size() {
    let dir = tempfile::tempdir().unwrap();
    let options = MdbxOptions {
        page_size: 16 * KIB,
        ..Default::default()
    };
    drop(MdbxStore::open_with_options(dir.path(), options).unwrap());
    let db = MdbxStore::open(dir.path()).unwrap();
    assert_eq!(page_size(&db), 16 * KIB);
}

#[test]
fn default_map_ceiling_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    assert_eq!(map_size(&db), DEFAULT_MAX_MAP_SIZE);
}

#[test]
fn configured_map_ceiling_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let options = MdbxOptions {
        max_map_size: 64 * MIB,
        ..Default::default()
    };
    let db = MdbxStore::open_with_options(dir.path(), options).unwrap();
    assert_eq!(map_size(&db), 64 * MIB);
}
