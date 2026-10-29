use crate::*;
use std::{cell::Cell, ops::Bound::Unbounded, rc::Rc};

struct FailingTransaction(Rc<Cell<usize>>);
struct FailingCursor(Rc<Cell<usize>>);

impl ReadCursor for FailingCursor {
    fn prev(&mut self) -> Result<Option<Entry>> {
        self.next()
    }
    fn next(&mut self) -> Result<Option<Entry>> {
        let count = self.0.get();
        self.0.set(count + 1);
        if count == 0 {
            Ok(Some((b"a".to_vec(), vec![])))
        } else {
            Err(StorageError::Backend(
                std::io::Error::other("injected read failure").into(),
            ))
        }
    }

    fn with_next<Output>(
        &mut self,
        inspect: impl FnOnce(Option<(&[u8], &[u8])>) -> Output,
    ) -> crate::Result<Output> {
        let row = self.next()?;
        Ok(inspect(
            row.as_ref().map(|(k, v)| (k.as_slice(), v.as_slice())),
        ))
    }
}

impl ReadTransaction for FailingTransaction {
    type Cursor<'tx> = FailingCursor;
    fn get(&self, _: Table, _: &[u8]) -> Result<Option<Vec<u8>>> {
        unreachable!()
    }
    fn cursor(&self, _: Table, _: &[u8]) -> Result<Self::Cursor<'_>> {
        Ok(FailingCursor(self.0.clone()))
    }

    fn with_value<Output>(
        &self,
        table: crate::Table,
        key: &[u8],
        inspect: impl FnOnce(Option<&[u8]>) -> Output,
    ) -> crate::Result<Output> {
        let _ = (table, key, inspect);

        unreachable!()
    }
}

#[test]
fn scan_returns_error_once_and_never_advances_again() {
    let calls = Rc::new(Cell::new(0));
    let tx = FailingTransaction(calls.clone());
    let mut scan = scan(&tx, Table("test"), Unbounded, Unbounded).unwrap();
    assert!(scan.next().unwrap().is_ok());
    assert!(matches!(scan.next(), Some(Err(StorageError::Backend(_)))));
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
    assert_eq!(calls.get(), 2);
}
