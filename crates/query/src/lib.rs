//! Queries over committed state: [`Queries::query`] and [`Queries::count`].
//!
//! A [`QueryOp`] or [`CountOp`] is plain data the caller builds around a
//! [`Filter`]. Leaves are typed: `amount = CellValue::from_i32(100)` reads only
//! the `i32` run of `amount`, so a cell of another type under the same name is
//! treated as absent. Only attribute cells are indexed, so only they match.
//!
//! Queries read committed state: the head (`None`), or a named commit. The
//! whole request is admitted before any state read. The filter's match set and
//! the returned records are then read from one storage snapshot. Opening it
//! reads the head, so a requested commit other than the head fails there,
//! after admission. Filter
//! evaluation itself never touches storage: it reads the index through a
//! private seam, so it is unit-tested with a fake.
//!
//! Callers reach queries through `golemdb_api::Database`, which builds its
//! [`Queries`] over the store its branch manager writes through:
//!
//! ```
//! use golemdb_api::{
//!     Api, CellLimits, CellNameRef, CellValue, Database, Filter, Genesis, HashAlgorithm,
//!     QueryOp, ReadTarget, RecordKey, RecordOp,
//! };
//!
//! let limits = CellLimits { max_cell_name_len: 32, max_str_len: 64, max_bytes_len: 64 };
//! let db = Database::open_memory(&Genesis::new(HashAlgorithm::Keccak256, limits))?;
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
//! let page = db.query(None, QueryOp::from_filter(filter))?;
//! assert_eq!(page.records, [db.get(ReadTarget::Head, RecordOp::get(key))?]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;
mod eval;
mod plan;
mod posting_source;
mod queries;
mod query_op;

pub use error::{QueryError, Result};
pub use queries::Queries;
pub use query_op::{Budget, CountOp, Direction, Filter, QueryOp, QueryPage, SortKey};
