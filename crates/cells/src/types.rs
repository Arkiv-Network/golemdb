//! [`CellType`]: the 7-bit type-id space, decoded.

use crate::error::CellValueParseError;
use crate::order::decode_float;

/// Whether a cell participates in the index. Type and kind are independent,
/// except that variable-length bytes can only be a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Field,
    Attribute,
}

/// The `w` of a `4 · 2^w`-byte family: 4, 8, 16 or 32 bytes. A family's four
/// members sit at consecutive type ids `base + w`, e.g. `u32..u256` at 12–15.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    W4,
    W8,
    W16,
    W32,
}

impl Width {
    const fn from_w(w: u8) -> Self {
        match w {
            0 => Self::W4,
            1 => Self::W8,
            2 => Self::W16,
            _ => Self::W32,
        }
    }

    pub const fn bytes(self) -> usize {
        4 << (self as u8)
    }

    /// The decimal places a `dec` of this width carries.
    pub const fn decimal_scale(self) -> u8 {
        match self {
            Self::W4 => 4,
            Self::W8 => 6,
            Self::W16 | Self::W32 => 18,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatWidth {
    F32,
    F64,
}

/// A cell's type.
///
/// | id    | type                   | value bytes | stored form      |
/// | ----- | ---------------------- | ----------- | ---------------- |
/// | 0     | *absent marker*        |             |                  |
/// | 1     | `bool`                 | 1           | `00` / `01`      |
/// | 2     | `str`                  | var         | UTF-8            |
/// | 3     | `bytes` (field-only)   | var         | as-is            |
/// | 4     | `bytes20`              | 20          | as-is            |
/// | 8–11  | `bytes4..32`           | 4·2^w       | as-is            |
/// | 12–15 | `u32..u256`            | 4·2^w       | big-endian       |
/// | 16–19 | `i32..i256`            | 4·2^w       | sign bit flipped |
/// | 20–23 | `dec32..dec256`        | 4·2^w       | sign bit flipped |
/// | 24–25 | `f32` `f64`            | 4 / 8       | IEEE total order |
/// | 28–29 | `date32` `timestamp64` | 4 / 8       | sign bit flipped |
///
/// Every other id up to 127 is reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellType {
    Bool,
    Str,
    /// Field-only: no order, so never indexable.
    Bytes,
    /// Address width; its own id because 20 is not `4 · 2^w`.
    Bytes20,
    FixedBytes(Width),
    Uint(Width),
    Int(Width),
    /// A signed integer at the fixed scale [`Width::decimal_scale`].
    Decimal(Width),
    Float(FloatWidth),
    /// Days since the Unix epoch.
    Date32,
    /// Microseconds since the Unix epoch.
    Timestamp64,
}

impl CellType {
    pub const fn id(self) -> u8 {
        match self {
            Self::Bool => 1,
            Self::Str => 2,
            Self::Bytes => 3,
            Self::Bytes20 => 4,
            Self::FixedBytes(w) => 8 + w as u8,
            Self::Uint(w) => 12 + w as u8,
            Self::Int(w) => 16 + w as u8,
            Self::Decimal(w) => 20 + w as u8,
            Self::Float(FloatWidth::F32) => 24,
            Self::Float(FloatWidth::F64) => 25,
            Self::Date32 => 28,
            Self::Timestamp64 => 29,
        }
    }

    /// Decode a 7-bit type id.
    pub const fn from_id(id: u8) -> Result<Self, CellValueParseError> {
        match id {
            0 => Err(CellValueParseError::AbsentTag),
            1 => Ok(Self::Bool),
            2 => Ok(Self::Str),
            3 => Ok(Self::Bytes),
            4 => Ok(Self::Bytes20),
            8..=11 => Ok(Self::FixedBytes(Width::from_w(id - 8))),
            12..=15 => Ok(Self::Uint(Width::from_w(id - 12))),
            16..=19 => Ok(Self::Int(Width::from_w(id - 16))),
            20..=23 => Ok(Self::Decimal(Width::from_w(id - 20))),
            24 => Ok(Self::Float(FloatWidth::F32)),
            25 => Ok(Self::Float(FloatWidth::F64)),
            28 => Ok(Self::Date32),
            29 => Ok(Self::Timestamp64),
            _ => Err(CellValueParseError::ReservedType(id)),
        }
    }

    /// The value width in bytes, or `None` for the variable-width `str` and
    /// `bytes`.
    pub const fn width(self) -> Option<usize> {
        match self {
            Self::Str | Self::Bytes => None,
            Self::Bool => Some(1),
            Self::Bytes20 => Some(20),
            Self::FixedBytes(w) | Self::Uint(w) | Self::Int(w) | Self::Decimal(w) => {
                Some(w.bytes())
            }
            Self::Float(FloatWidth::F32) | Self::Date32 => Some(4),
            Self::Float(FloatWidth::F64) | Self::Timestamp64 => Some(8),
        }
    }

    /// Check that `value` is a valid stored value of this type.
    pub fn validate(self, value: &[u8]) -> Result<(), CellValueParseError> {
        match self.width() {
            Some(n) if value.len() != n => {
                return Err(CellValueParseError::LengthMismatch {
                    ty: self,
                    expected: n,
                    actual: value.len(),
                });
            }
            _ => {}
        }
        match self {
            Self::Bool if value[0] > 1 => Err(CellValueParseError::InvalidBool(value[0])),
            Self::Str => match core::str::from_utf8(value) {
                Ok(_) => Ok(()),
                Err(e) => Err(CellValueParseError::InvalidUtf8 {
                    valid_up_to: e.valid_up_to(),
                }),
            },
            Self::Float(w) => {
                // The length check above makes `try_into` infallible, and f32
                // widens to f64 exactly, NaN and -0.0 included.
                let x = match w {
                    FloatWidth::F32 => {
                        f32::from_be_bytes(decode_float(value.try_into().unwrap())) as f64
                    }
                    FloatWidth::F64 => f64::from_be_bytes(decode_float(value.try_into().unwrap())),
                };
                if x.is_nan() {
                    Err(CellValueParseError::FloatNaN)
                } else if x == 0.0 && x.is_sign_negative() {
                    Err(CellValueParseError::NegativeZero)
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}
