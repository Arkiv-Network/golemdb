//! A real in-memory index, written the way the engine writes it.
//!
//! Terms on the write side come from [`IndexTerm::from_cell`] over stored,
//! order-encoded cells, never from [`Value::term`]. Queries therefore only
//! match if both paths agree on every type's encoding.

#![allow(dead_code)] // Each test crate uses a different subset.

use golemdb_cells::{CellValue, CellValueRef, encode_float, flip_sign};
use golemdb_index::{INDEX_TRIE_PATH_BYTES, Index, IndexTerm, PostingChange};
use golemdb_merkle::{Keccak256Hasher, RootRef};
use golemdb_query::{IndexSource, Query, QueryError, Value, execute};
use golemdb_storage::{Database, MemoryDatabase, WriteTransaction};
use proptest::prelude::*;

const HASHER: Keccak256Hasher = Keccak256Hasher;

/// The payload the engine stores for `value`: native bytes in their order
/// encoding, built with the cells crate's own transforms.
pub fn stored_cell(value: &Value) -> CellValue {
    let bytes: Vec<u8> = match value {
        Value::Bool(v) => vec![u8::from(*v)],
        Value::Str(v) => v.as_bytes().to_vec(),
        Value::Bytes4(v) => v.to_vec(),
        Value::Bytes8(v) => v.to_vec(),
        Value::Bytes16(v) => v.to_vec(),
        Value::Bytes20(v) => v.to_vec(),
        Value::Bytes32(v) | Value::U256(v) => v.to_vec(),
        Value::U32(v) => v.to_be_bytes().to_vec(),
        Value::U64(v) => v.to_be_bytes().to_vec(),
        Value::U128(v) => v.to_be_bytes().to_vec(),
        Value::I32(v) | Value::Dec32(v) | Value::Date32(v) => flip_sign(v.to_be_bytes()).to_vec(),
        Value::I64(v) | Value::Dec64(v) | Value::Timestamp64(v) => {
            flip_sign(v.to_be_bytes()).to_vec()
        }
        Value::I128(v) | Value::Dec128(v) => flip_sign(v.to_be_bytes()).to_vec(),
        Value::I256(v) | Value::Dec256(v) => flip_sign(*v).to_vec(),
        Value::F32(v) => encode_float(v.to_be_bytes()).to_vec(),
        Value::F64(v) => encode_float(v.to_be_bytes()).to_vec(),
    };
    CellValueRef::new(value.cell_type(), &bytes, true)
        .expect("valid stored cell")
        .into()
}

/// The term the engine's write path derives for `field = value`.
pub fn written_term(field: &str, value: &Value) -> IndexTerm {
    IndexTerm::from_cell(field, stored_cell(value).as_view())
        .expect("valid term")
        .expect("indexable cell")
}

pub fn add(record_id: u64, field: &str, value: &Value) -> PostingChange {
    PostingChange::Add {
        term: written_term(field, value),
        record_id,
    }
}

pub fn remove(record_id: u64, field: &str, value: &Value) -> PostingChange {
    PostingChange::Remove {
        term: written_term(field, value),
        record_id,
    }
}

/// A committed index in a [`MemoryDatabase`].
pub struct Store {
    db: MemoryDatabase,
    index: Index<'static, Keccak256Hasher>,
    root: RootRef<INDEX_TRIE_PATH_BYTES>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            db: MemoryDatabase::new(),
            index: Index::new(&HASHER),
            root: RootRef::Empty,
        }
    }
}

impl Store {
    /// Apply one batch and commit it.
    pub fn apply(&mut self, changes: impl IntoIterator<Item = PostingChange>) {
        let mut tx = self.db.begin_write().expect("write transaction");
        self.root = self
            .index
            .apply(&mut tx, self.root, changes)
            .expect("index update")
            .root;
        tx.commit().expect("commit");
    }

    /// Run `query` against one committed read snapshot.
    pub fn query(&self, query: impl Into<Query>) -> Result<Vec<u64>, QueryError> {
        let tx = self.db.begin_read().expect("read transaction");
        let ids = execute(&IndexSource::new(&self.index, &tx), &query.into())?;
        Ok(ids.iter().collect())
    }
}

/// Any representable value except NaN, which has no term.
pub fn any_value() -> impl Strategy<Value = Value> {
    let f32s = any::<f32>().prop_filter("NaN has no term", |v| !v.is_nan());
    let f64s = any::<f64>().prop_filter("NaN has no term", |v| !v.is_nan());
    prop_oneof![
        any::<bool>().prop_map(Value::Bool),
        any::<String>().prop_map(Value::Str),
        any::<[u8; 4]>().prop_map(Value::Bytes4),
        any::<[u8; 8]>().prop_map(Value::Bytes8),
        any::<[u8; 16]>().prop_map(Value::Bytes16),
        any::<[u8; 20]>().prop_map(Value::Bytes20),
        any::<[u8; 32]>().prop_map(Value::Bytes32),
        any::<u32>().prop_map(Value::U32),
        any::<u64>().prop_map(Value::U64),
        any::<u128>().prop_map(Value::U128),
        any::<[u8; 32]>().prop_map(Value::U256),
        any::<i32>().prop_map(Value::I32),
        any::<i64>().prop_map(Value::I64),
        any::<i128>().prop_map(Value::I128),
        any::<[u8; 32]>().prop_map(Value::I256),
        any::<i32>().prop_map(Value::Dec32),
        any::<i64>().prop_map(Value::Dec64),
        any::<i128>().prop_map(Value::Dec128),
        any::<[u8; 32]>().prop_map(Value::Dec256),
        f32s.prop_map(Value::F32),
        f64s.prop_map(Value::F64),
        any::<i32>().prop_map(Value::Date32),
        any::<i64>().prop_map(Value::Timestamp64),
    ]
}

/// A value from a tiny domain per type, so random records and queries collide.
/// `n` in `-2..=2` picks the value; the same `n` across types gives values
/// whose native bits often coincide, which typed predicates must tell apart.
pub fn small_value() -> impl Strategy<Value = Value> {
    (0u8..23, -2i8..=2).prop_map(|(kind, n)| {
        let wide = i128::from(n);
        let byte = n as u8;
        let mut word = [0u8; 32];
        word[31] = byte;
        let float = [f64::NEG_INFINITY, -1.5, -0.0, 0.0, f64::INFINITY][(n + 2) as usize];
        match kind {
            0 => Value::Bool(n > 0),
            1 => Value::Str(["", "a", "ab", "é", "b"][(n + 2) as usize].into()),
            2 => Value::Bytes4([byte; 4]),
            3 => Value::Bytes8([byte; 8]),
            4 => Value::Bytes16([byte; 16]),
            5 => Value::Bytes20([byte; 20]),
            6 => Value::Bytes32(word),
            7 => Value::U32(byte.into()),
            8 => Value::U64(byte.into()),
            9 => Value::U128(byte.into()),
            10 => Value::U256(word),
            11 => Value::I32(n.into()),
            12 => Value::I64(n.into()),
            13 => Value::I128(wide),
            14 => {
                let mut v = [if n < 0 { 0xff } else { 0 }; 32];
                v[16..].copy_from_slice(&wide.to_be_bytes());
                Value::I256(v)
            }
            15 => Value::Dec32(n.into()),
            16 => Value::Dec64(n.into()),
            17 => Value::Dec128(wide),
            18 => Value::Dec256(word),
            19 => Value::F32(float as f32),
            20 => Value::F64(float),
            21 => Value::Date32(n.into()),
            _ => Value::Timestamp64(n.into()),
        }
    })
}
