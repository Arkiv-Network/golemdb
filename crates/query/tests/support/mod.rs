//! A real in-memory index, written the way the engine writes it.
//!
//! Terms on the write side come from [`IndexTerm::from_cell`] over attribute
//! cells, exactly as seal derives them; filters look the same values up
//! through admission.

#![allow(dead_code)] // Each test crate uses a different subset.

use golemdb_cells::{CellKind, CellValue};
use golemdb_index::{INDEX_TRIE_PATH_BYTES, Index, IndexTerm, PostingChange};
use golemdb_merkle::{Keccak256Hasher, RootRef};
use golemdb_query::{Filter, IndexSource, QueryError, evaluate};
use golemdb_storage::{MemoryStore, Store, WriteTransaction};
use proptest::prelude::*;

const HASHER: Keccak256Hasher = Keccak256Hasher;

/// The term seal derives when a record holds `field = value` as an attribute.
pub fn written_term(field: &str, value: &CellValue) -> IndexTerm {
    let attribute = value
        .clone()
        .with_kind(CellKind::Attribute)
        .expect("indexable type");
    IndexTerm::from_cell(field, attribute.as_view())
        .expect("valid term")
        .expect("attribute")
}

pub fn add(record_id: u64, field: &str, value: &CellValue) -> PostingChange {
    PostingChange::Add {
        term: written_term(field, value),
        record_id,
    }
}

pub fn remove(record_id: u64, field: &str, value: &CellValue) -> PostingChange {
    PostingChange::Remove {
        term: written_term(field, value),
        record_id,
    }
}

/// A committed index in a [`MemoryStore`].
pub struct TestIndex {
    store: MemoryStore,
    index: Index<'static, Keccak256Hasher>,
    root: RootRef<INDEX_TRIE_PATH_BYTES>,
}

impl Default for TestIndex {
    fn default() -> Self {
        Self {
            store: MemoryStore::new(),
            index: Index::new(&HASHER),
            root: RootRef::Empty,
        }
    }
}

impl TestIndex {
    /// Apply one batch and commit it.
    pub fn apply(&mut self, changes: impl IntoIterator<Item = PostingChange>) {
        let mut tx = self.store.begin_write().expect("write transaction");
        self.root = self
            .index
            .apply(&mut tx, self.root, changes)
            .expect("index update")
            .root;
        tx.commit().expect("commit");
    }

    /// Evaluate `filter` against one committed read snapshot.
    pub fn evaluate(&self, filter: &Filter) -> Result<Vec<u64>, QueryError> {
        let tx = self.store.begin_read().expect("read transaction");
        let ids = evaluate(&IndexSource::new(&self.index, &tx), filter)?;
        Ok(ids.treemap().iter().collect())
    }
}

fn f32_value(value: f32) -> CellValue {
    CellValue::from_f32(value).expect("not NaN")
}

fn f64_value(value: f64) -> CellValue {
    CellValue::from_f64(value).expect("not NaN")
}

/// Any value of an indexable type. NaN is excluded: it has no `CellValue`.
pub fn any_value() -> impl Strategy<Value = CellValue> {
    let f32s = any::<f32>().prop_filter("NaN is no value", |v| !v.is_nan());
    let f64s = any::<f64>().prop_filter("NaN is no value", |v| !v.is_nan());
    prop_oneof![
        any::<bool>().prop_map(CellValue::from_bool),
        any::<String>().prop_map(|v| CellValue::from_str(&v)),
        any::<[u8; 4]>().prop_map(CellValue::from_bytes4),
        any::<[u8; 8]>().prop_map(CellValue::from_bytes8),
        any::<[u8; 16]>().prop_map(CellValue::from_bytes16),
        any::<[u8; 20]>().prop_map(CellValue::from_bytes20),
        any::<[u8; 32]>().prop_map(CellValue::from_bytes32),
        any::<u32>().prop_map(CellValue::from_u32),
        any::<u64>().prop_map(CellValue::from_u64),
        any::<u128>().prop_map(CellValue::from_u128),
        any::<[u8; 32]>().prop_map(CellValue::from_u256_be),
        any::<i32>().prop_map(CellValue::from_i32),
        any::<i64>().prop_map(CellValue::from_i64),
        any::<i128>().prop_map(CellValue::from_i128),
        any::<[u8; 32]>().prop_map(CellValue::from_i256_be),
        any::<i32>().prop_map(CellValue::from_dec32_unscaled),
        any::<i64>().prop_map(CellValue::from_dec64_unscaled),
        any::<i128>().prop_map(CellValue::from_dec128_unscaled),
        any::<[u8; 32]>().prop_map(CellValue::from_dec256_unscaled_be),
        f32s.prop_map(f32_value),
        f64s.prop_map(f64_value),
        any::<i32>().prop_map(CellValue::from_date32),
        any::<i64>().prop_map(CellValue::from_timestamp64),
    ]
}

/// A value from a tiny domain per type, so random records and filters collide.
/// `n` in `-2..=2` picks the value; the same `n` across types gives values
/// whose native bits often coincide, which typed leaves must tell apart.
pub fn small_value() -> impl Strategy<Value = CellValue> {
    (0u8..23, -2i8..=2).prop_map(|(kind, n)| {
        let wide = i128::from(n);
        let byte = n as u8;
        let mut word = [0u8; 32];
        word[31] = byte;
        let float = [f64::NEG_INFINITY, -1.5, -0.0, 0.0, f64::INFINITY][(n + 2) as usize];
        match kind {
            0 => CellValue::from_bool(n > 0),
            1 => CellValue::from_str(["", "a", "ab", "é", "b"][(n + 2) as usize]),
            2 => CellValue::from_bytes4([byte; 4]),
            3 => CellValue::from_bytes8([byte; 8]),
            4 => CellValue::from_bytes16([byte; 16]),
            5 => CellValue::from_bytes20([byte; 20]),
            6 => CellValue::from_bytes32(word),
            7 => CellValue::from_u32(byte.into()),
            8 => CellValue::from_u64(byte.into()),
            9 => CellValue::from_u128(byte.into()),
            10 => CellValue::from_u256_be(word),
            11 => CellValue::from_i32(n.into()),
            12 => CellValue::from_i64(n.into()),
            13 => CellValue::from_i128(wide),
            14 => {
                let mut v = [if n < 0 { 0xff } else { 0 }; 32];
                v[16..].copy_from_slice(&wide.to_be_bytes());
                CellValue::from_i256_be(v)
            }
            15 => CellValue::from_dec32_unscaled(n.into()),
            16 => CellValue::from_dec64_unscaled(n.into()),
            17 => CellValue::from_dec128_unscaled(wide),
            18 => CellValue::from_dec256_unscaled_be(word),
            19 => f32_value(float as f32),
            20 => f64_value(float),
            21 => CellValue::from_date32(n.into()),
            _ => CellValue::from_timestamp64(n.into()),
        }
    })
}
