//! Query evaluation over the transactional index.
//!
//! A [`Query`] is plain data the caller builds; [`execute`] resolves it to the
//! IDs of the matching records. Predicates are typed: `amount = I32(100)`
//! reads only the `i32` run of `amount`, so a cell of another type under the
//! same name is treated as absent.
//!
//! All index reads go through [`PostingSource`]. [`IndexSource`] adapts an
//! [`Index`](golemdb_index::Index) at one read snapshot; the evaluation itself
//! never touches storage.
//!
//! ```
//! use golemdb_cells::CellType;
//! use golemdb_index::{Index, IndexTerm, PostingChange};
//! use golemdb_merkle::{Keccak256Hasher, RootRef};
//! use golemdb_query::{IndexSource, Predicate, Query, execute};
//! use golemdb_storage::{Database, MemoryDatabase, WriteTransaction};
//!
//! let db = MemoryDatabase::new();
//! let hasher = Keccak256Hasher;
//! let index = Index::new(&hasher);
//! let term = IndexTerm::new("color", CellType::Str, b"blue")?;
//! let mut tx = db.begin_write()?;
//! index.apply(&mut tx, RootRef::Empty, [
//!     PostingChange::Add { term, record_id: 42 },
//! ])?;
//! tx.commit()?;
//!
//! let read = db.begin_read()?;
//! let query = Query::from(Predicate::eq("color", "blue"));
//! let ids = execute(&IndexSource::new(&index, &read), &query)?;
//! assert_eq!(ids.iter().collect::<Vec<_>>(), [42]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;
mod eval;
mod plan;
mod query;
mod source;
mod value;

pub use error::{QueryError, Result};
pub use eval::execute;
pub use query::{Op, Predicate, Query};
pub use source::{IndexSource, PostingSource};
pub use value::Value;
