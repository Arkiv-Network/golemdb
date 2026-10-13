//! Queries over committed state: [`Queries::query`] and [`Queries::count`].
//!
//! A [`QueryOp`] or [`CountOp`] is plain data the caller builds around a
//! [`Filter`]. Leaves are typed: `amount = CellValue::from_i32(100)` reads only
//! the `i32` run of `amount`, so a cell of another type under the same name is
//! treated as absent. Only attribute cells are indexed, so only they match.
//!
//! The target is checked first, then the whole request is admitted before any
//! state read; the filter's match set and the returned records are then read
//! from one storage snapshot. Filter evaluation itself never touches storage:
//! it reads the index through a private seam, so it is unit-tested with a fake.
//!
//! ```
//! use golemdb_api::{
//!     Api, CellLimits, CellValue, Database, Genesis, HashAlgorithm, OpenConfig, ReadTarget,
//!     RecordKey, RecordOp,
//! };
//! use golemdb_branch::Branches;
//! use golemdb_cells::CellNameRef;
//! use golemdb_merkle::Keccak256Hasher;
//! use golemdb_query::{Filter, Queries, QueryOp};
//! use golemdb_storage::MemoryStore;
//!
//! // A database writes and commits; queries read the same store.
//! let store = MemoryStore::new();
//! let limits = CellLimits { max_cell_name_len: 32, max_str_len: 64, max_bytes_len: 64 };
//! let genesis = Genesis::new(HashAlgorithm::Keccak256, limits);
//! let db = Database::from_store(store.clone(), &OpenConfig::new(genesis))?;
//! let queries = Queries::new(Branches::new(store, Keccak256Hasher)?);
//!
//! let key = RecordKey([1; 32]);
//! let branch = db.begin()?;
//! db.create(
//!     branch,
//!     RecordOp::create(key).attribute("color", CellValue::from_str("blue"))?,
//! )?;
//! db.commit(branch)?;
//!
//! let filter = Filter::Eq {
//!     field: CellNameRef::parse_user(b"color", 32)?.into(),
//!     value: CellValue::from_str("blue"),
//! };
//! let page = queries.query(ReadTarget::Head, QueryOp::filter(filter))?;
//! assert_eq!(page.records, [db.get(ReadTarget::Head, RecordOp::get(key))?]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;
mod eval;
mod plan;
mod queries;
mod query_op;
mod source;

pub use error::{QueryError, Result};
pub use queries::Queries;
pub use query_op::{Budget, CountOp, Direction, Filter, QueryOp, QueryPage, SortKey};
