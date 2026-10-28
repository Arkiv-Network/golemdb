//! Queries over committed state: [`Queries::query`] and [`Queries::count`].
//!
//! A [`QueryOp`] or [`CountOp`] is plain data the caller builds around a
//! [`Filter`]. Leaves are typed: `amount = EqValue::of(marker::I32, 100)` reads
//! only the `i32` run of `amount`, so a cell of another type under the same
//! name is treated as absent. Only attribute cells are indexed, so only they match.
//!
//! Queries select the committed head (`None`) or a named commit before admission.
//! The selected snapshot supplies the pricing head, index reads and materialized
//! records, including on failure. Non-head historical reads remain unsupported.
//! Report variants retain that context for API receipts. Explicit budgets are
//! stored but not enforced yet. Filter evaluation uses a private index seam.
//!
//! Callers reach queries through `golemdb_api::Database`, which builds its
//! [`Queries`] over the store its branch manager writes through:
//!
//! ```
//! use golemdb_api::{
//!     Api, CellLimits, CellNameRef, CellValue, Database, EqValue, Filter, Genesis,
//!     HashAlgorithm, QueryOp, ReadTarget, RecordKey, RecordOp, marker::Str,
//! };
//!
//! let limits = CellLimits { max_cell_name_len: 32, max_str_len: 64, max_bytes_len: 64 };
//! let db = Database::open_memory(&Genesis::new(
//!     HashAlgorithm::Keccak256,
//!     limits,
//!     golemdb_api::RecordLimits { max_cells: 100, max_indexed_cells: 100 },
//!     None,
//!     golemdb_api::PricingConfig::zero(),
//! ))?;
//!
//! let key = RecordKey([1; 32]);
//! let branch = db.begin()?;
//! db.create(
//!     branch,
//!     RecordOp::create(key, golemdb_api::Budget::Limited(u64::MAX)).attribute("color", CellValue::from_str("blue")),
//! ).into_result()?;
//! db.commit(branch)?;
//!
//! let filter = Filter::Eq {
//!     field: CellNameRef::parse_user(b"color", 32)?.into(),
//!     value: EqValue::of(Str, "blue"),
//! };
//! let page = db.query(None, QueryOp::from_filter(filter, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;
//! assert_eq!(page.records, [db.get(ReadTarget::Head, RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))).into_result()?]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;
mod eval;
mod plan;
mod posting_source;
mod queries;
mod query_op;

pub use error::{QueryError, Result};
pub use golemdb_metering::Budget;
pub use queries::Queries;
pub use query_op::{
    Bounds, CountOp, Direction, EqValue, ExistsType, Filter, InValues, QueryOp, QueryPage, SortKey,
    ValuesError,
};
