//! A real in-memory database, used only through the public API: records are
//! written and committed, so the index is built by seal exactly as in
//! production, and then queried.

#![allow(dead_code)] // Each test crate uses a different subset.

use golemdb_api::{
    Api, ApiError, Bounds, CellLimits, CountOp, Database, EqValue, Filter, Genesis, HashAlgorithm,
    InValues, QueryOp, RecordKey, RecordOp, op,
};
use std::{cmp::Ordering, ops::Bound};

use golemdb_cells::{CellNameRef, CellType, CellValue, FloatWidth, Width};
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
        let page = self.db.query(None, QueryOp::from_filter(filter))?;
        Ok(page.records.into_iter().map(|record| record.key).collect())
    }

    pub fn count(&self, filter: Filter) -> Result<u64, ApiError> {
        self.db.count(None, CountOp::filter(filter))
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

/// Raw, so tests can also build names the grammar rejects. Through the
/// runtime path: the value must be of an indexable type.
pub fn eq(field: &str, value: CellValue) -> Filter {
    Filter::Eq {
        field: CellNameRef::raw(field.as_bytes()).into(),
        value: EqValue::try_from(value).expect("a value of an indexable type"),
    }
}

fn f32_value(value: f32) -> CellValue {
    CellValue::from_f32(value).expect("not NaN")
}

fn f64_value(value: f64) -> CellValue {
    CellValue::from_f64(value).expect("not NaN")
}

/// Raw, so tests can also build names the grammar rejects. Through the
/// runtime path: the values must be of one indexable type.
pub fn is_in(field: &str, values: impl IntoIterator<Item = CellValue>) -> Filter {
    Filter::In {
        field: CellNameRef::raw(field.as_bytes()).into(),
        values: InValues::try_from(values.into_iter().collect::<Vec<_>>())
            .expect("values of one indexable type"),
    }
}

/// A range of the bounds' type: at least one side must hold a value. See
/// [`range_of`] for two unbounded sides.
pub fn range(field: &str, lower: Bound<CellValue>, upper: Bound<CellValue>) -> Filter {
    let ty = [&lower, &upper]
        .into_iter()
        .find_map(|bound| match bound {
            Bound::Included(value) | Bound::Excluded(value) => Some(value.cell_type()),
            Bound::Unbounded => None,
        })
        .expect("a bounded side names the type; use `range_of` otherwise");
    range_of(field, ty, lower, upper)
}

/// Raw, so tests can also build names the grammar rejects. Through the
/// runtime path: the bounds must be of type `ty`, an indexable one.
pub fn range_of(
    field: &str,
    ty: CellType,
    lower: Bound<CellValue>,
    upper: Bound<CellValue>,
) -> Filter {
    Filter::Range {
        field: CellNameRef::raw(field.as_bytes()).into(),
        bounds: Bounds::try_new(ty, lower, upper).expect("bounds of an indexable type"),
    }
}

/// Raw, so tests can also build names the grammar rejects.
pub fn prefix(field: &str, prefix: &str) -> Filter {
    Filter::Prefix {
        field: CellNameRef::raw(field.as_bytes()).into(),
        prefix: prefix.to_owned(),
    }
}

/// Whether `value` lies between `lower` and `upper` in the native order of
/// its type: a value of another type than a bound never does.
pub fn is_between(value: &CellValue, lower: &Bound<CellValue>, upper: &Bound<CellValue>) -> bool {
    let above = match lower {
        Bound::Included(low) => native_cmp(value, low).is_some_and(Ordering::is_ge),
        Bound::Excluded(low) => native_cmp(value, low).is_some_and(Ordering::is_gt),
        Bound::Unbounded => true,
    };
    let below = match upper {
        Bound::Included(high) => native_cmp(value, high).is_some_and(Ordering::is_le),
        Bound::Excluded(high) => native_cmp(value, high).is_some_and(Ordering::is_lt),
        Bound::Unbounded => true,
    };
    above && below
}

/// The order of two values of one type, `None` across types. Compares the
/// values as native Rust values, without the cells' order encoding, so it is
/// a reference for the index's byte order.
pub fn native_cmp(a: &CellValue, b: &CellValue) -> Option<Ordering> {
    if a.cell_type() != b.cell_type() {
        return None;
    }
    let order = native(a).partial_cmp(&native(b));
    Some(order.expect("values of one type compare: NaN is rejected"))
}

/// A cell value as a native one. Only values of one type are compared.
#[derive(Debug, PartialEq, PartialOrd)]
enum Native<'a> {
    Bool(bool),
    Str(&'a str),
    /// Fixed bytes and `u256`, big-endian: compared byte by byte.
    Bytes(Vec<u8>),
    Unsigned(u128),
    Signed(i128),
    /// A 256-bit two's complement, big-endian: negative values first, then
    /// byte by byte.
    Signed256 {
        non_negative: bool,
        be: [u8; 32],
    },
    Float(f64),
}

fn native(value: &CellValue) -> Native<'_> {
    const OWN_TYPE: &str = "accessor of the value's own type";
    let signed256 = |be: [u8; 32]| Native::Signed256 {
        non_negative: be[0] & 0x80 == 0,
        be,
    };
    match value.cell_type() {
        CellType::Bool => Native::Bool(value.as_bool().expect(OWN_TYPE)),
        CellType::Str => Native::Str(value.as_str().expect(OWN_TYPE)),
        CellType::Bytes => panic!("bytes has no order"),
        CellType::Bytes20 => Native::Bytes(value.as_bytes20().expect(OWN_TYPE).into()),
        CellType::FixedBytes(Width::W4) => Native::Bytes(value.as_bytes4().expect(OWN_TYPE).into()),
        CellType::FixedBytes(Width::W8) => Native::Bytes(value.as_bytes8().expect(OWN_TYPE).into()),
        CellType::FixedBytes(Width::W16) => {
            Native::Bytes(value.as_bytes16().expect(OWN_TYPE).into())
        }
        CellType::FixedBytes(Width::W32) => {
            Native::Bytes(value.as_bytes32().expect(OWN_TYPE).into())
        }
        CellType::Uint(Width::W4) => Native::Unsigned(value.as_u32().expect(OWN_TYPE).into()),
        CellType::Uint(Width::W8) => Native::Unsigned(value.as_u64().expect(OWN_TYPE).into()),
        CellType::Uint(Width::W16) => Native::Unsigned(value.as_u128().expect(OWN_TYPE)),
        CellType::Uint(Width::W32) => Native::Bytes(value.as_u256_be().expect(OWN_TYPE).into()),
        CellType::Int(Width::W4) => Native::Signed(value.as_i32().expect(OWN_TYPE).into()),
        CellType::Int(Width::W8) => Native::Signed(value.as_i64().expect(OWN_TYPE).into()),
        CellType::Int(Width::W16) => Native::Signed(value.as_i128().expect(OWN_TYPE)),
        CellType::Int(Width::W32) => signed256(value.as_i256_be().expect(OWN_TYPE)),
        CellType::Decimal(Width::W4) => {
            Native::Signed(value.as_dec32_unscaled().expect(OWN_TYPE).into())
        }
        CellType::Decimal(Width::W8) => {
            Native::Signed(value.as_dec64_unscaled().expect(OWN_TYPE).into())
        }
        CellType::Decimal(Width::W16) => {
            Native::Signed(value.as_dec128_unscaled().expect(OWN_TYPE))
        }
        CellType::Decimal(Width::W32) => signed256(value.as_dec256_unscaled_be().expect(OWN_TYPE)),
        CellType::Float(FloatWidth::F32) => Native::Float(value.as_f32().expect(OWN_TYPE).into()),
        CellType::Float(FloatWidth::F64) => Native::Float(value.as_f64().expect(OWN_TYPE)),
        CellType::Date32 => Native::Signed(value.as_date32().expect(OWN_TYPE).into()),
        CellType::Timestamp64 => Native::Signed(value.as_timestamp64().expect(OWN_TYPE).into()),
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

/// A random type, and a lower and an upper bound of it, each unbounded,
/// included or excluded, from [`small_value`]'s domain: often equal,
/// inverted, or both unbounded.
pub fn small_bounds_of_one_type()
-> impl Strategy<Value = (CellType, Bound<CellValue>, Bound<CellValue>)> {
    (0..TYPES).prop_flat_map(|ty| {
        (
            Just(typed_value(ty, 0).cell_type()),
            small_bound(ty),
            small_bound(ty),
        )
    })
}

fn small_bound(ty: u8) -> impl Strategy<Value = Bound<CellValue>> {
    prop_oneof![
        1 => Just(Bound::Unbounded),
        2 => (-2i8..=2).prop_map(move |n| Bound::Included(typed_value(ty, n))),
        2 => (-2i8..=2).prop_map(move |n| Bound::Excluded(typed_value(ty, n))),
    ]
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
