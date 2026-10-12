//! Query evaluation over the transactional index.
//!
//! A query's [`Filter`] is plain data the caller builds; [`evaluate`] resolves
//! it to its match set, the IDs of the matching records. Leaves are typed:
//! `amount = CellValue::from_i32(100)` reads only the `i32` run of `amount`,
//! so a cell of another type under the same name is treated as absent.
//!
//! Admission validates the whole filter before any state read. All index
//! reads then go through [`PostingSource`]. [`IndexSource`] adapts an
//! [`Index`](golemdb_index::Index) at one read snapshot; the evaluation itself
//! never touches storage.
//!
//! ```
//! use golemdb_cells::{CellNameRef, CellType, CellValue};
//! use golemdb_index::{Index, IndexTerm, PostingChange};
//! use golemdb_merkle::{Keccak256Hasher, RootRef};
//! use golemdb_query::{Filter, IndexSource, evaluate};
//! use golemdb_storage::{MemoryStore, Store, WriteTransaction};
//!
//! let store = MemoryStore::new();
//! let hasher = Keccak256Hasher;
//! let index = Index::new(&hasher);
//! let term = IndexTerm::new("color", CellType::Str, b"blue")?;
//! let mut tx = store.begin_write()?;
//! index.apply(&mut tx, RootRef::Empty, [
//!     PostingChange::Add { term, record_id: 42 },
//! ])?;
//! tx.commit()?;
//!
//! let read = store.begin_read()?;
//! let filter = Filter::Eq {
//!     field: CellNameRef::parse_user(b"color", 32)?.into(),
//!     value: CellValue::from_str("blue"),
//! };
//! let ids = evaluate(&IndexSource::new(&index, &read), &filter)?;
//! assert_eq!(ids.treemap().iter().collect::<Vec<_>>(), [42]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;
mod eval;
mod filter;
mod plan;
mod source;

pub use error::{QueryError, Result};
pub use eval::evaluate;
pub use filter::Filter;
pub use source::{IndexSource, PostingSource};
