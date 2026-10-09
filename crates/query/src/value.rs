//! [`Value`]: a typed query literal.

use std::borrow::Cow;

use golemdb_cells::{CellType, FloatWidth, Width};
use golemdb_index::{IndexTerm, TermError};

/// A literal a predicate compares against, carrying its cell type.
///
/// The type is part of what a predicate asserts: `amount = I32(100)` never
/// matches an `amount` stored as `u32` or `str`. Each variant maps to exactly
/// one indexable [`CellType`], so the field-only `bytes` type cannot be
/// expressed. Numbers are native values; 256-bit numbers are big-endian bytes
/// (two's complement when signed), and decimals are their unscaled integers.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Str(String),
    Bytes4([u8; 4]),
    Bytes8([u8; 8]),
    Bytes16([u8; 16]),
    Bytes20([u8; 20]),
    Bytes32([u8; 32]),
    U32(u32),
    U64(u64),
    U128(u128),
    U256([u8; 32]),
    I32(i32),
    I64(i64),
    I128(i128),
    I256([u8; 32]),
    Dec32(i32),
    Dec64(i64),
    Dec128(i128),
    Dec256([u8; 32]),
    F32(f32),
    F64(f64),
    /// Days since the Unix epoch.
    Date32(i32),
    /// Microseconds since the Unix epoch.
    Timestamp64(i64),
}

impl Value {
    pub fn cell_type(&self) -> CellType {
        self.native().0
    }

    /// The index term `field = self` is stored under. [`IndexTerm::new`] owns
    /// the order encoding, so query terms and written terms cannot drift apart.
    pub fn term(&self, field: &str) -> Result<IndexTerm, TermError> {
        let (ty, bytes) = self.native();
        IndexTerm::new(field, ty, &bytes)
    }

    /// Cell type and native big-endian bytes, as [`IndexTerm::new`] expects.
    fn native(&self) -> (CellType, Cow<'_, [u8]>) {
        let owned = |bytes: &[u8]| Cow::Owned(bytes.to_vec());
        match self {
            Self::Bool(value) => (CellType::Bool, owned(&[u8::from(*value)])),
            Self::Str(value) => (CellType::Str, Cow::Borrowed(value.as_bytes())),
            Self::Bytes4(value) => (CellType::FixedBytes(Width::W4), Cow::Borrowed(value)),
            Self::Bytes8(value) => (CellType::FixedBytes(Width::W8), Cow::Borrowed(value)),
            Self::Bytes16(value) => (CellType::FixedBytes(Width::W16), Cow::Borrowed(value)),
            Self::Bytes20(value) => (CellType::Bytes20, Cow::Borrowed(value)),
            Self::Bytes32(value) => (CellType::FixedBytes(Width::W32), Cow::Borrowed(value)),
            Self::U32(value) => (CellType::Uint(Width::W4), owned(&value.to_be_bytes())),
            Self::U64(value) => (CellType::Uint(Width::W8), owned(&value.to_be_bytes())),
            Self::U128(value) => (CellType::Uint(Width::W16), owned(&value.to_be_bytes())),
            Self::U256(value) => (CellType::Uint(Width::W32), Cow::Borrowed(value)),
            Self::I32(value) => (CellType::Int(Width::W4), owned(&value.to_be_bytes())),
            Self::I64(value) => (CellType::Int(Width::W8), owned(&value.to_be_bytes())),
            Self::I128(value) => (CellType::Int(Width::W16), owned(&value.to_be_bytes())),
            Self::I256(value) => (CellType::Int(Width::W32), Cow::Borrowed(value)),
            Self::Dec32(value) => (CellType::Decimal(Width::W4), owned(&value.to_be_bytes())),
            Self::Dec64(value) => (CellType::Decimal(Width::W8), owned(&value.to_be_bytes())),
            Self::Dec128(value) => (CellType::Decimal(Width::W16), owned(&value.to_be_bytes())),
            Self::Dec256(value) => (CellType::Decimal(Width::W32), Cow::Borrowed(value)),
            Self::F32(value) => (
                CellType::Float(FloatWidth::F32),
                owned(&value.to_be_bytes()),
            ),
            Self::F64(value) => (
                CellType::Float(FloatWidth::F64),
                owned(&value.to_be_bytes()),
            ),
            Self::Date32(value) => (CellType::Date32, owned(&value.to_be_bytes())),
            Self::Timestamp64(value) => (CellType::Timestamp64, owned(&value.to_be_bytes())),
        }
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Str(value.into())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

impl From<u32> for Value {
    fn from(value: u32) -> Self {
        Self::U32(value)
    }
}

impl From<u64> for Value {
    fn from(value: u64) -> Self {
        Self::U64(value)
    }
}

impl From<u128> for Value {
    fn from(value: u128) -> Self {
        Self::U128(value)
    }
}

impl From<i32> for Value {
    fn from(value: i32) -> Self {
        Self::I32(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::I64(value)
    }
}

impl From<i128> for Value {
    fn from(value: i128) -> Self {
        Self::I128(value)
    }
}

impl From<f32> for Value {
    fn from(value: f32) -> Self {
        Self::F32(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::F64(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_types() -> Vec<(Value, CellType)> {
        vec![
            (Value::Bool(true), CellType::Bool),
            (Value::Str("x".into()), CellType::Str),
            (Value::Bytes4([1; 4]), CellType::FixedBytes(Width::W4)),
            (Value::Bytes8([1; 8]), CellType::FixedBytes(Width::W8)),
            (Value::Bytes16([1; 16]), CellType::FixedBytes(Width::W16)),
            (Value::Bytes20([1; 20]), CellType::Bytes20),
            (Value::Bytes32([1; 32]), CellType::FixedBytes(Width::W32)),
            (Value::U32(1), CellType::Uint(Width::W4)),
            (Value::U64(1), CellType::Uint(Width::W8)),
            (Value::U128(1), CellType::Uint(Width::W16)),
            (Value::U256([1; 32]), CellType::Uint(Width::W32)),
            (Value::I32(1), CellType::Int(Width::W4)),
            (Value::I64(1), CellType::Int(Width::W8)),
            (Value::I128(1), CellType::Int(Width::W16)),
            (Value::I256([1; 32]), CellType::Int(Width::W32)),
            (Value::Dec32(1), CellType::Decimal(Width::W4)),
            (Value::Dec64(1), CellType::Decimal(Width::W8)),
            (Value::Dec128(1), CellType::Decimal(Width::W16)),
            (Value::Dec256([1; 32]), CellType::Decimal(Width::W32)),
            (Value::F32(1.0), CellType::Float(FloatWidth::F32)),
            (Value::F64(1.0), CellType::Float(FloatWidth::F64)),
            (Value::Date32(1), CellType::Date32),
            (Value::Timestamp64(1), CellType::Timestamp64),
        ]
    }

    #[test]
    fn every_variant_maps_to_its_cell_type() {
        for (value, ty) in all_types() {
            assert_eq!(value.cell_type(), ty, "{value:?}");
            let term = value.term("f").expect("valid term");
            let prefix = IndexTerm::prefix("f", ty).expect("indexable type");
            assert!(term.as_bytes().starts_with(&prefix), "{value:?}");
        }
    }

    #[test]
    fn same_bits_of_different_types_are_different_terms() {
        let one = 1u32.to_be_bytes();
        let terms = [
            Value::U32(1),
            Value::I32(1),
            Value::Dec32(1),
            Value::Date32(1),
            Value::Bytes4(one),
            Value::F32(f32::from_bits(1)),
        ]
        .map(|value| value.term("f").expect("valid term"));
        for (i, a) in terms.iter().enumerate() {
            for b in &terms[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn negative_zero_is_zero() {
        assert_eq!(Value::F32(-0.0).term("f"), Value::F32(0.0).term("f"));
        assert_eq!(Value::F64(-0.0).term("f"), Value::F64(0.0).term("f"));
    }

    #[test]
    fn nan_has_no_term() {
        assert_eq!(Value::F32(f32::NAN).term("f"), Err(TermError::NaN));
        assert_eq!(Value::F64(f64::NAN).term("f"), Err(TermError::NaN));
        let negative_nan = f64::from_bits(f64::NAN.to_bits() | 1 << 63);
        assert_eq!(Value::F64(negative_nan).term("f"), Err(TermError::NaN));
    }

    #[test]
    fn infinities_have_terms() {
        for value in [
            Value::F32(f32::INFINITY),
            Value::F32(f32::NEG_INFINITY),
            Value::F64(f64::INFINITY),
            Value::F64(f64::NEG_INFINITY),
        ] {
            assert!(value.term("f").is_ok(), "{value:?}");
        }
    }

    #[test]
    fn invalid_field_names_have_no_term() {
        for name in ["", "$", "1a", "a b", "a\0b", "é", "_a"] {
            assert_eq!(
                Value::I32(1).term(name),
                Err(TermError::InvalidName),
                "{name:?}"
            );
        }
        for name in ["a", "$owner", "a.b", "a-b:c_d", "A9"] {
            assert!(Value::I32(1).term(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn native_conversions_pick_the_obvious_type() {
        assert_eq!(Value::from(true), Value::Bool(true));
        assert_eq!(Value::from("a"), Value::Str("a".into()));
        assert_eq!(Value::from(String::from("a")), Value::Str("a".into()));
        assert_eq!(Value::from(1u32), Value::U32(1));
        assert_eq!(Value::from(1u64), Value::U64(1));
        assert_eq!(Value::from(1u128), Value::U128(1));
        assert_eq!(Value::from(1i32), Value::I32(1));
        assert_eq!(Value::from(1i64), Value::I64(1));
        assert_eq!(Value::from(1i128), Value::I128(1));
        assert_eq!(Value::from(1.0f32), Value::F32(1.0));
        assert_eq!(Value::from(1.0f64), Value::F64(1.0));
    }
}
