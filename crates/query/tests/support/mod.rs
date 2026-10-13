//! A real in-memory database, used only through the public API: records are
//! written and committed, so the index is built by seal exactly as in
//! production, and then queried.

#![allow(dead_code)] // Each test crate uses a different subset.

use golemdb_api::{
    Api, ApiError, CellLimits, CountOp, Database, Filter, Genesis, HashAlgorithm, QueryOp,
    ReadTarget, RecordKey, RecordOp, op,
};
use golemdb_cells::{CellNameRef, CellValue};
use proptest::prelude::*;

pub struct TestDb {
    pub db: Database,
}

impl Default for TestDb {
    fn default() -> Self {
        let limits = CellLimits {
            max_cell_name_len: 32,
            max_str_len: 4096,
            max_bytes_len: 64,
        };
        let genesis = Genesis::new(HashAlgorithm::Keccak256, limits);
        Self {
            db: Database::open_memory(&genesis).expect("genesis"),
        }
    }
}

impl TestDb {
    /// Create `records` in one branch and commit it.
    pub fn create(&self, records: impl IntoIterator<Item = RecordOp<op::Create>>) {
        let branch = self.db.begin().expect("branch");
        for record in records {
            self.db.create(branch, record).expect("create");
        }
        self.db.commit(branch).expect("commit");
    }

    /// Keys of the records matching `filter` at the head, in query order.
    pub fn keys(&self, filter: Filter) -> Result<Vec<RecordKey>, ApiError> {
        let page = self.db.query(ReadTarget::Head, QueryOp::filter(filter))?;
        Ok(page.records.into_iter().map(|record| record.key).collect())
    }

    pub fn count(&self, filter: Filter) -> Result<u64, ApiError> {
        self.db.count(ReadTarget::Head, CountOp::filter(filter))
    }
}

pub fn key(n: u64) -> RecordKey {
    let mut bytes = [0xab; 32];
    bytes[24..].copy_from_slice(&n.to_be_bytes());
    RecordKey(bytes)
}

/// A record whose cells are all attributes, so all of them are indexed.
pub fn record(key: RecordKey, attributes: &[(&str, CellValue)]) -> RecordOp<op::Create> {
    attributes
        .iter()
        .fold(RecordOp::create(key), |op, (name, value)| {
            op.attribute(name, value.clone()).expect("valid attribute")
        })
}

/// Raw, so tests can also build names the grammar rejects.
pub fn eq(field: &str, value: CellValue) -> Filter {
    Filter::Eq {
        field: CellNameRef::raw(field.as_bytes()).into(),
        value,
    }
}

fn f32_value(value: f32) -> CellValue {
    CellValue::from_f32(value).expect("not NaN")
}

fn f64_value(value: f64) -> CellValue {
    CellValue::from_f64(value).expect("not NaN")
}

/// Raw, so tests can also build names the grammar rejects.
pub fn is_in(field: &str, values: impl IntoIterator<Item = CellValue>) -> Filter {
    Filter::In {
        field: CellNameRef::raw(field.as_bytes()).into(),
        values: values.into_iter().collect(),
    }
}

/// Number of types [`typed_value`] picks from.
const TYPES: u8 = 23;

/// A value from a tiny domain per type, so random records and filters collide.
pub fn small_value() -> impl Strategy<Value = CellValue> {
    (0..TYPES, -2i8..=2).prop_map(|(ty, n)| typed_value(ty, n))
}

/// `len` values of one random type, from [`small_value`]'s domain, so
/// duplicates are common.
pub fn small_values_of_one_type(
    len: std::ops::Range<usize>,
) -> impl Strategy<Value = Vec<CellValue>> {
    (0..TYPES).prop_flat_map(move |ty| {
        prop::collection::vec(
            (-2i8..=2).prop_map(move |n| typed_value(ty, n)),
            len.clone(),
        )
    })
}

/// Type `ty` (`0..TYPES`), value `n` in `-2..=2`. The same `n` across types
/// gives values whose native bits often coincide, which typed leaves must
/// tell apart.
fn typed_value(ty: u8, n: i8) -> CellValue {
    let wide = i128::from(n);
    let byte = n as u8;
    let mut word = [0u8; 32];
    word[31] = byte;
    let float = [f64::NEG_INFINITY, -1.5, -0.0, 0.0, f64::INFINITY][(n + 2) as usize];
    match ty {
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
}
