//! GolemDB cells: complete keys and typed values with owned and borrowed forms.
//!
//! # Wire format
//!
//! ```text
//! [ indexable: 1 bit | type id: 7 bits ] [ value … ]
//!
//! u64 7, indexable   8D 00 00 00 00 00 00 00 07
//! str "hi"           02 68 69
//! ```
//!
//! Each storage row or slice contains exactly one cell. Fixed-width types
//! require their declared width; `str` and `bytes` consume the remaining slice.
//! No value has a length prefix. Concatenating cells requires external boundaries.
//!
//! Type id `0x00` is not a type. It is the branch overlay's "absent" marker
//! ([`CellValueParseError::AbsentTag`]); a tombstone is `Option<CellValue>::None`.
//!
//! # Order encoding
//!
//! Stored value bytes sort as their values do, so a value is copied verbatim
//! into its index term `cellName ‖ 0x00 ‖ typeTag ‖ value`, and a cursor walk
//! over that is a range query. A value has one byte form everywhere.
//!
//! | stored form                         | types                                                 |
//! | ----------------------------------- | ----------------------------------------------------- |
//! | natural bytes                       | `bool`, `str`, `bytes20`, `bytes4..32`, `u32..u256`   |
//! | sign bit flipped ([`flip_sign`])    | `i32..i256`, `dec32..dec256`, `date32`, `timestamp64` |
//! | IEEE total order ([`encode_float`]) | `f32`, `f64`                                          |
//! | not indexable                       | `bytes`                                               |
//!
//! ```text
//! i32 -1   FF FF FF FF  stored  7F FF FF FF
//! i32 100  00 00 00 64  stored  80 00 00 64   7F… < 80…, so -1 < 100
//! ```
//!
//! Stored NaN and negative zero are rejected; [`encode_float`] normalizes
//! input negative zero to positive zero. Strings use raw UTF-8 throughout.
//!
//! Decimals are signed integers at a fixed scale per width
//! ([`Width::decimal_scale`]): `dec32` 4, `dec64` 6, `dec128` 18, `dec256` 18.
//! `dec256` is fixed by Arkiv (wei); the others await sign-off.
//!
//! # Typed construction
//!
//! [`CellValue::from_i32`] and the other `from_<type>` constructors accept natural
//! typed values and handle canonical encoding. They create fields by default;
//! [`CellValue::with_kind`] enables indexing when the type supports it. No caller
//! bit transformations or indexing flags are needed. Float constructors reject
//! NaN and normalize negative zero; encoded-byte parsing remains strict.
//!
//! ```
//! use golemdb_cells::{CellKind, CellValue};
//! let price = CellValue::from_i32(50).with_kind(CellKind::Attribute)?;
//! let description = CellValue::from_str("A product");
//! assert_eq!(price.as_i32(), Some(50));
//! assert_eq!(price.encoded_bytes(), &[0x90, 0x80, 0, 0, 0x32]);
//! assert_eq!(description.kind(), CellKind::Field);
//! # Ok::<(), golemdb_cells::CellValueParseError>(())
//! ```
//!
//! Decimals accept unscaled integers (`from_dec32_unscaled(123_450)` represents
//! 12.3450 at scale 4). The 256-bit constructors use natural big-endian arrays,
//! signed values in two's complement. Dates use days and timestamps microseconds
//! since the Unix epoch. Deployment limits are still checked at record admission.
//! The former two-argument `from_u64`/`from_bytes32` constructors now take only
//! the value; use `with_kind(CellKind::Attribute)` to request an attribute.
//!
//! # Addressing and ownership
//!
//! [`CellName`] is the within-record name (including raw reserved-record keys).
//! [`CellKey`] is the complete address: `record_id` as eight BE bytes, then the
//! name. [`CellValue`] keeps read results alive independently of a storage
//! transaction; [`CellValueRef`] borrows payloads for inspection without copying.
//! [`CellLimits`] checks genesis-configured admission policy separately from
//! representation validation.
//!
//! ```
//! use golemdb_cells::{CellKey, CellLimits, CellValueRef, CellValue};
//!
//! let limits = CellLimits {
//!     max_cell_name_len: 64, max_str_len: 1024, max_bytes_len: 65536,
//! };
//! let key = CellKey::new(42, limits.parse_user_name(b"$owner")?);
//! assert_eq!(&key.encode()[8..], b"$owner");
//! let owned = CellValue::parse(vec![0x82, b'h', b'i'])?;
//! limits.validate_value(owned.as_view())?;
//! assert_eq!(owned.as_str(), Some("hi"));
//! assert_eq!(CellValueRef::parse(owned.encoded_bytes())?, owned.as_view());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Transactional storage
//!
//! [`Cells`] reads owned values, scans a record, and applies a net batch to both
//! the `Cell` table and its branch-only `CellTrie`. The caller supplies the
//! matching root and owns the transaction; it can write history and the head in
//! the same transaction. Deployment admission and index updates are engine work.
//!
//! ```
//! use golemdb_cells::{CellChange, CellKey, CellNameRef, CellValue, Cells};
//! use golemdb_merkle::{Keccak256Hasher, RootRef};
//! use golemdb_storage::{Store, MemoryStore, WriteTransaction};
//!
//! let store = MemoryStore::new();
//! let hasher = Keccak256Hasher;
//! let cells = Cells::new(&hasher);
//! let key = CellKey::new(42, CellNameRef::parse_user(b"status", 64)?);
//! let mut tx = store.begin_write()?;
//! let update = cells.apply(&mut tx, RootRef::Empty, [CellChange::Put {
//!     key: key.clone(), value: CellValue::parse(b"\x02ready".to_vec())?,
//! }])?;
//! tx.commit()?;
//! let read = store.begin_read()?;
//! assert_eq!(cells.get(&read, &key)?.unwrap().as_str(), Some("ready"));
//! assert_eq!(cells.reopen(&read, update.root.hash(&hasher))?, update.root);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

/// Domain byte for cell leaf hashes.
pub const CELL_LEAF_DOMAIN: u8 = 0x00;
/// Domain byte for CellTrie branch hashes.
pub const CELL_BRANCH_DOMAIN: u8 = 0x01;

mod cell_trie;
mod cells;
mod config;
mod error;
mod key;
mod name;
mod order;
pub mod system;
pub mod tables;
mod types;
mod value;

#[cfg(test)]
mod tests;

pub use cells::{CellChange, CellReader, CellScan, CellValueChange, Cells, CellsUpdate};
pub use config::{CellLimitError, CellLimits};
pub use error::{CellError, CellValueParseError, Result};
pub use key::{CellKey, CellKeyError};
pub use name::{CellName, CellNameError, CellNameRef, reserved};
pub use order::{decode_float, encode_float, flip_sign};
pub use types::{CellKind, CellType, FloatWidth, Width};
pub use value::{CellValue, CellValueRef};

/// Cell trie paths are full hash digests of the encoded cell key.
pub const CELL_TRIE_PATH_BYTES: usize = size_of::<golemdb_merkle::Hash>();

/// The metadata byte's high bit: whether the cell is indexable. The low 7
/// bits are the type id.
pub(crate) const INDEXABLE_BIT: u8 = 0x80;

/// The metadata byte’s low seven bits: the cell type id.
pub(crate) const TYPE_MASK: u8 = !INDEXABLE_BIT;
