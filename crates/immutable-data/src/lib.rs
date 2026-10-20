//! Typed physical immutable-data storage with caller-owned metadata.
//!
//! This crate reuses cell payload codecs without cell indexing or size limits.
//! Rows normalize values to fields; their storage payloads omit cell metadata.
//! A schema supplies the types when decoding those payloads. Projection selects
//! and validates column names before a backend fetches only the selected values.
//! Read rows share owned layout metadata, supporting named native access through
//! `get::<T>(name)` and borrowed dynamic access through `value(name)`. Append rows
//! constructed with `new` are unnamed and use the ordered `values()` accessor.
//!
//! Genesis declarations resolve omitted types to bytes and encode deterministically
//! for the API opener's committed parameters. [`ImmutableDataStorage::in_memory`]
//! and [`ImmutableDataStorage::open`] select memory or NippyJar storage, with
//! separate key/range helpers using caller-owned KV transactions. Branch staging
//! and publication coordination live in `golemdb-branch`. Physical recovery uses
//! caller-supplied durable commit ranges without reading or writing database head.
//! The API facade exposes typed rows and committed reads through these types.
//! These types do not depend on API or branch types.
//!
//! ```
//! use golemdb_cells::{CellType, CellValue, Width};
//! use golemdb_immutable_data::{
//!     ImmutableDataColumn, ImmutableDataSchema, ImmutableDataRow,
//!     ImmutableDataReadOptions,
//! };
//!
//! let schema = ImmutableDataSchema::new("headers", vec![
//!     ImmutableDataColumn::new("block_height").with_type(CellType::Uint(Width::W8)),
//!     ImmutableDataColumn::new("block_body"), // bytes by default
//! ])?;
//! let row = ImmutableDataRow::new(vec![
//!     CellValue::from_u64(42), CellValue::from_bytes(b"header"),
//! ]);
//! schema.validate_row(&row)?;
//! let payloads: Vec<_> = row.payloads().collect();
//! assert_eq!(payloads[0], 42_u64.to_be_bytes()); // no cell tag
//! assert_eq!(schema.decode_row(&payloads)?.values(), row.values());
//!
//! let projection = schema.projection(
//!     &ImmutableDataReadOptions::columns(["block_body", "block_height"]),
//! )?;
//! assert_eq!(projection.indices(), &[1, 0]);
//! // A backend fetches only these payloads, in the requested order.
//! let selected = projection.decode(&[payloads[1], payloads[0]])?;
//! let height: u64 = selected.get("block_height")?;
//! let body = selected.get::<Vec<u8>>("block_body")?; // owned copy
//! assert_eq!((body.as_slice(), height), (b"header".as_slice(), 42));
//! let borrowed = selected.value("block_body")?.as_bytes().unwrap();
//! assert_eq!(borrowed, b"header");
//! for (name, value) in selected.iter()? {
//!     println!("{name}: {:?}", value.cell_type());
//! }
//! # Ok::<(), golemdb_immutable_data::ImmutableDataError>(())
//! ```

mod backend;
mod config;
mod error;
mod files;
mod memory;
pub mod metadata;
mod reader;
mod row;
mod schema;
mod storage;
mod storage_error;
mod types;

#[cfg(test)]
mod tests;

pub use config::{
    ImmutableDataCompression, ImmutableDataConfig, ImmutableDataConfigError, ImmutableDataSegment,
    MAX_COLUMNS, MAX_SCHEMA_BYTES, MAX_SEGMENTS,
};
pub use error::{ImmutableDataError, Result};
pub use reader::ImmutableDataReader;
pub use row::{FromImmutableDataValue, ImmutableDataRow};
pub use schema::{ImmutableDataColumn, ImmutableDataProjection, ImmutableDataSchema};
pub use storage::ImmutableDataStorage;
pub use storage_error::{ImmutableDataStorageError, StorageResult};
pub use types::{
    ImmutableDataAddress, ImmutableDataKey, ImmutableDataOrdinal, ImmutableDataReadOptions,
};
