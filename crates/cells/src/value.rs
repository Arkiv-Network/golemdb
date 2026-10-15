//! [`CellValue`] owns encoded bytes; [`CellValueRef`] borrows their payload.
//! Both expose the same metadata and typed accessors.

use crate::error::CellValueParseError;
use crate::order::{decode_float, encode_float, flip_sign};
use crate::types::{CellKind, CellType, FloatWidth, Width};
use crate::{INDEXABLE_BIT, TYPE_MASK};

/// A cell: a type, its stored value bytes, and whether it is indexable.
/// Borrows the value straight out of the storage buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellValueRef<'a> {
    ty: CellType,
    value: &'a [u8],
    indexable: bool,
}

/// A validated, owned `typeTag ‖ value` buffer for storage and CRUD results.
/// Borrow with `as_view` while inspecting, indexing or hashing the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellValue {
    bytes: Vec<u8>,
    ty: CellType,
}

impl CellValue {
    /// Validate once and take ownership of a complete encoded cell without
    /// copying its buffer. Deployment limits are checked separately at admission.
    pub fn parse(bytes: Vec<u8>) -> Result<Self, CellValueParseError> {
        let ty = CellValueRef::parse(&bytes)?.cell_type();
        Ok(Self { bytes, ty })
    }

    /// A borrowed view, without allocation or revalidation of the payload.
    pub fn as_view(&self) -> CellValueRef<'_> {
        CellValueRef {
            ty: self.ty,
            value: &self.bytes[1..],
            indexable: self.bytes[0] & INDEXABLE_BIT != 0,
        }
    }

    /// The indexable bit over the type id.
    pub fn metadata(&self) -> u8 {
        self.bytes[0]
    }

    pub const fn cell_type(&self) -> CellType {
        self.ty
    }

    /// The ordered payload bytes, without the type tag.
    pub fn value(&self) -> &[u8] {
        &self.bytes[1..]
    }

    pub fn is_indexable(&self) -> bool {
        self.bytes[0] & INDEXABLE_BIT != 0
    }

    pub fn kind(&self) -> CellKind {
        self.as_view().kind()
    }

    /// Change only the kind, retaining the type and encoded payload. Variable-
    /// length bytes cannot become an attribute. Deployment limits are unchanged.
    pub fn with_kind(mut self, kind: CellKind) -> Result<Self, CellValueParseError> {
        if kind == CellKind::Attribute && self.ty == CellType::Bytes {
            return Err(CellValueParseError::NotIndexable);
        }
        self.bytes[0] = self.ty.id()
            | match kind {
                CellKind::Field => 0,
                CellKind::Attribute => INDEXABLE_BIT,
            };
        Ok(self)
    }

    /// See [`CellValueRef::as_bool`]. Returns `None` for other cell types.
    pub fn as_bool(&self) -> Option<bool> {
        self.as_view().as_bool()
    }

    /// See [`CellValueRef::as_str`]. Returns `None` for other cell types.
    pub fn as_str(&self) -> Option<&str> {
        self.as_view().as_str()
    }

    /// See [`CellValueRef::as_bytes`]. Returns `None` for other cell types.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        self.as_view().as_bytes()
    }

    /// See [`CellValueRef::as_bytes4`]. Returns `None` for other cell types.
    pub fn as_bytes4(&self) -> Option<[u8; 4]> {
        self.as_view().as_bytes4()
    }

    /// See [`CellValueRef::as_bytes8`]. Returns `None` for other cell types.
    pub fn as_bytes8(&self) -> Option<[u8; 8]> {
        self.as_view().as_bytes8()
    }

    /// See [`CellValueRef::as_bytes16`]. Returns `None` for other cell types.
    pub fn as_bytes16(&self) -> Option<[u8; 16]> {
        self.as_view().as_bytes16()
    }

    /// See [`CellValueRef::as_bytes20`]. Returns `None` for other cell types.
    pub fn as_bytes20(&self) -> Option<[u8; 20]> {
        self.as_view().as_bytes20()
    }

    /// See [`CellValueRef::as_bytes32`]. Returns `None` for other cell types.
    pub fn as_bytes32(&self) -> Option<[u8; 32]> {
        self.as_view().as_bytes32()
    }

    /// See [`CellValueRef::as_u32`]. Returns `None` for other cell types.
    pub fn as_u32(&self) -> Option<u32> {
        self.as_view().as_u32()
    }

    /// See [`CellValueRef::as_u64`]. Returns `None` for other cell types.
    pub fn as_u64(&self) -> Option<u64> {
        self.as_view().as_u64()
    }

    /// See [`CellValueRef::as_u128`]. Returns `None` for other cell types.
    pub fn as_u128(&self) -> Option<u128> {
        self.as_view().as_u128()
    }

    /// See [`CellValueRef::as_u256_be`]. Returns `None` for other cell types.
    pub fn as_u256_be(&self) -> Option<[u8; 32]> {
        self.as_view().as_u256_be()
    }

    /// See [`CellValueRef::as_i32`]. Returns `None` for other cell types.
    pub fn as_i32(&self) -> Option<i32> {
        self.as_view().as_i32()
    }

    /// See [`CellValueRef::as_i64`]. Returns `None` for other cell types.
    pub fn as_i64(&self) -> Option<i64> {
        self.as_view().as_i64()
    }

    /// See [`CellValueRef::as_i128`]. Returns `None` for other cell types.
    pub fn as_i128(&self) -> Option<i128> {
        self.as_view().as_i128()
    }

    /// See [`CellValueRef::as_i256_be`]. Returns `None` for other cell types.
    pub fn as_i256_be(&self) -> Option<[u8; 32]> {
        self.as_view().as_i256_be()
    }

    /// See [`CellValueRef::as_dec32_unscaled`]. Returns `None` for other cell types.
    pub fn as_dec32_unscaled(&self) -> Option<i32> {
        self.as_view().as_dec32_unscaled()
    }

    /// See [`CellValueRef::as_dec64_unscaled`]. Returns `None` for other cell types.
    pub fn as_dec64_unscaled(&self) -> Option<i64> {
        self.as_view().as_dec64_unscaled()
    }

    /// See [`CellValueRef::as_dec128_unscaled`]. Returns `None` for other cell types.
    pub fn as_dec128_unscaled(&self) -> Option<i128> {
        self.as_view().as_dec128_unscaled()
    }

    /// See [`CellValueRef::as_dec256_unscaled_be`]. Returns `None` for other cell types.
    pub fn as_dec256_unscaled_be(&self) -> Option<[u8; 32]> {
        self.as_view().as_dec256_unscaled_be()
    }

    /// See [`CellValueRef::as_f32`]. Returns `None` for other cell types.
    pub fn as_f32(&self) -> Option<f32> {
        self.as_view().as_f32()
    }

    /// See [`CellValueRef::as_f64`]. Returns `None` for other cell types.
    pub fn as_f64(&self) -> Option<f64> {
        self.as_view().as_f64()
    }

    /// See [`CellValueRef::as_date32`]. Returns `None` for other cell types.
    pub fn as_date32(&self) -> Option<i32> {
        self.as_view().as_date32()
    }

    /// See [`CellValueRef::as_timestamp64`]. Returns `None` for other cell types.
    pub fn as_timestamp64(&self) -> Option<i64> {
        self.as_view().as_timestamp64()
    }

    // Typed constructors produce fields; with_kind selects indexing separately.
    // Only typed constructors call this with payloads already known to satisfy
    // the codec. Untrusted storage bytes must still go through parse/new.
    fn field_from_typed(ty: CellType, value: &[u8]) -> Self {
        CellValueRef {
            ty,
            value,
            indexable: false,
        }
        .into()
    }

    pub fn from_bool(value: bool) -> Self {
        Self::field_from_typed(CellType::Bool, &[u8::from(value)])
    }

    /// Copy valid UTF-8 as a field. Deployment length limits are checked later.
    // This explicitly constructs the str cell type; it does not parse textual
    // representations of arbitrary cells as a FromStr implementation would imply.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(value: &str) -> Self {
        Self::field_from_typed(CellType::Str, value.as_bytes())
    }

    /// Copy arbitrary bytes as a field. Variable-length bytes cannot be indexed.
    pub fn from_bytes(value: &[u8]) -> Self {
        Self::field_from_typed(CellType::Bytes, value)
    }

    pub fn from_bytes4(value: [u8; 4]) -> Self {
        Self::field_from_typed(CellType::FixedBytes(Width::W4), &value)
    }

    pub fn from_bytes8(value: [u8; 8]) -> Self {
        Self::field_from_typed(CellType::FixedBytes(Width::W8), &value)
    }

    pub fn from_bytes16(value: [u8; 16]) -> Self {
        Self::field_from_typed(CellType::FixedBytes(Width::W16), &value)
    }

    pub fn from_bytes20(value: [u8; 20]) -> Self {
        Self::field_from_typed(CellType::Bytes20, &value)
    }

    pub fn from_bytes32(value: [u8; 32]) -> Self {
        Self::field_from_typed(CellType::FixedBytes(Width::W32), &value)
    }

    pub fn from_u32(value: u32) -> Self {
        Self::field_from_typed(CellType::Uint(Width::W4), &value.to_be_bytes())
    }

    pub fn from_u64(value: u64) -> Self {
        Self::field_from_typed(CellType::Uint(Width::W8), &value.to_be_bytes())
    }

    pub fn from_u128(value: u128) -> Self {
        Self::field_from_typed(CellType::Uint(Width::W16), &value.to_be_bytes())
    }

    /// Unsigned 256-bit integer in natural big-endian form; Rust has no u256.
    pub fn from_u256_be(value: [u8; 32]) -> Self {
        Self::field_from_typed(CellType::Uint(Width::W32), &value)
    }

    pub fn from_i32(value: i32) -> Self {
        Self::field_from_typed(CellType::Int(Width::W4), &flip_sign(value.to_be_bytes()))
    }

    pub fn from_i64(value: i64) -> Self {
        Self::field_from_typed(CellType::Int(Width::W8), &flip_sign(value.to_be_bytes()))
    }

    pub fn from_i128(value: i128) -> Self {
        Self::field_from_typed(CellType::Int(Width::W16), &flip_sign(value.to_be_bytes()))
    }

    /// Signed 256-bit integer in natural two's-complement big-endian form.
    /// The caller does not flip the sign bit; this constructor handles storage order.
    pub fn from_i256_be(value: [u8; 32]) -> Self {
        Self::field_from_typed(CellType::Int(Width::W32), &flip_sign(value))
    }

    /// A decimal with four places: 123_450 represents 12.3450.
    /// No rounding or scaling is performed.
    pub fn from_dec32_unscaled(value: i32) -> Self {
        Self::field_from_typed(
            CellType::Decimal(Width::W4),
            &flip_sign(value.to_be_bytes()),
        )
    }

    /// A decimal with six places; input is the signed unscaled integer.
    pub fn from_dec64_unscaled(value: i64) -> Self {
        Self::field_from_typed(
            CellType::Decimal(Width::W8),
            &flip_sign(value.to_be_bytes()),
        )
    }

    /// A decimal with eighteen places; input is the signed unscaled integer.
    pub fn from_dec128_unscaled(value: i128) -> Self {
        Self::field_from_typed(
            CellType::Decimal(Width::W16),
            &flip_sign(value.to_be_bytes()),
        )
    }

    /// A decimal with eighteen places. Input is the unscaled signed integer in
    /// natural two's-complement big-endian form, before storage-order encoding.
    pub fn from_dec256_unscaled_be(value: [u8; 32]) -> Self {
        Self::field_from_typed(CellType::Decimal(Width::W32), &flip_sign(value))
    }

    /// Reject NaN, normalize negative zero, and retain infinities.
    pub fn from_f32(value: f32) -> Result<Self, CellValueParseError> {
        if value.is_nan() {
            return Err(CellValueParseError::FloatNaN);
        }
        Ok(Self::field_from_typed(
            CellType::Float(FloatWidth::F32),
            &encode_float(value.to_be_bytes()),
        ))
    }

    /// Reject NaN, normalize negative zero, and retain infinities.
    pub fn from_f64(value: f64) -> Result<Self, CellValueParseError> {
        if value.is_nan() {
            return Err(CellValueParseError::FloatNaN);
        }
        Ok(Self::field_from_typed(
            CellType::Float(FloatWidth::F64),
            &encode_float(value.to_be_bytes()),
        ))
    }

    /// Signed days since the Unix epoch. This does not parse a calendar date.
    pub fn from_date32(days: i32) -> Self {
        Self::field_from_typed(CellType::Date32, &flip_sign(days.to_be_bytes()))
    }

    /// Signed microseconds since the Unix epoch. No timezone conversion is done.
    pub fn from_timestamp64(microseconds: i64) -> Self {
        Self::field_from_typed(
            CellType::Timestamp64,
            &flip_sign(microseconds.to_be_bytes()),
        )
    }

    /// The complete storage encoding, including the type tag.
    /// Use `value()` for the payload or `as_bytes()` for a typed bytes value.
    pub fn encoded_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Transfer ownership of the complete storage encoding, including its tag.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl From<CellValueRef<'_>> for CellValue {
    /// Copy a borrowed value into an independently owned encoded buffer.
    fn from(cell: CellValueRef<'_>) -> Self {
        Self {
            bytes: cell.encode(),
            ty: cell.ty,
        }
    }
}

impl<'a> CellValueRef<'a> {
    /// Build a cell from a stored value, validating it.
    pub fn new(
        ty: CellType,
        value: &'a [u8],
        indexable: bool,
    ) -> Result<Self, CellValueParseError> {
        if indexable && ty == CellType::Bytes {
            return Err(CellValueParseError::NotIndexable);
        }
        ty.validate(value)?;
        Ok(Self {
            ty,
            value,
            indexable,
        })
    }

    /// Decode one complete cell buffer. Strings and bytes consume the entire
    /// remainder; fixed-width types require exactly their declared width.
    /// The surrounding storage row or slice supplies the cell boundary.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, CellValueParseError> {
        let (&metadata, value) = bytes.split_first().ok_or(CellValueParseError::Empty)?;
        let ty = CellType::from_id(metadata & TYPE_MASK)?;
        Self::new(ty, value, metadata & INDEXABLE_BIT != 0)
    }

    /// Append this cell's wire bytes to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(self.metadata());
        out.extend_from_slice(self.value);
    }

    /// Encode the metadata byte followed by the stored payload.
    pub fn encode(&self) -> Vec<u8> {
        let capacity = self
            .value
            .len()
            .checked_add(1)
            .expect("encoded cell length exceeds usize::MAX");
        let mut out = Vec::with_capacity(capacity);
        self.encode_into(&mut out);
        out
    }

    /// The indexable bit over the type id.
    pub const fn metadata(&self) -> u8 {
        self.ty.id() | if self.indexable { INDEXABLE_BIT } else { 0 }
    }

    pub const fn cell_type(&self) -> CellType {
        self.ty
    }

    /// The stored value bytes.
    pub const fn value(&self) -> &'a [u8] {
        self.value
    }

    pub const fn is_indexable(&self) -> bool {
        self.indexable
    }

    pub const fn kind(&self) -> CellKind {
        if self.indexable {
            CellKind::Attribute
        } else {
            CellKind::Field
        }
    }

    // Typed accessors: `None` unless the cell holds exactly that type.

    /// The value as an array, if the cell is of type `want`.
    fn exact<const N: usize>(&self, want: CellType) -> Option<[u8; N]> {
        // `validate` fixed the width, and each caller asks for its type's own.
        (self.ty == want).then(|| self.value.try_into().unwrap())
    }

    pub fn as_bool(&self) -> Option<bool> {
        self.exact(CellType::Bool).map(|[b]| b == 1)
    }

    pub fn as_str(&self) -> Option<&'a str> {
        match self.ty {
            CellType::Str => core::str::from_utf8(self.value).ok(),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        (self.ty == CellType::Bytes).then_some(self.value)
    }

    pub fn as_bytes4(&self) -> Option<[u8; 4]> {
        self.exact(CellType::FixedBytes(Width::W4))
    }

    pub fn as_bytes8(&self) -> Option<[u8; 8]> {
        self.exact(CellType::FixedBytes(Width::W8))
    }

    pub fn as_bytes16(&self) -> Option<[u8; 16]> {
        self.exact(CellType::FixedBytes(Width::W16))
    }

    pub fn as_bytes20(&self) -> Option<[u8; 20]> {
        self.exact(CellType::Bytes20)
    }

    pub fn as_bytes32(&self) -> Option<[u8; 32]> {
        self.exact(CellType::FixedBytes(Width::W32))
    }

    pub fn as_u32(&self) -> Option<u32> {
        self.exact(CellType::Uint(Width::W4))
            .map(u32::from_be_bytes)
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.exact(CellType::Uint(Width::W8))
            .map(u64::from_be_bytes)
    }

    pub fn as_u128(&self) -> Option<u128> {
        self.exact(CellType::Uint(Width::W16))
            .map(u128::from_be_bytes)
    }

    /// Big-endian; Rust has no `u256`.
    pub fn as_u256_be(&self) -> Option<[u8; 32]> {
        self.exact(CellType::Uint(Width::W32))
    }

    // Signed types are stored sign-flipped (`7F FF FF FF` is -1); these
    // flip it back.

    pub fn as_i32(&self) -> Option<i32> {
        self.exact(CellType::Int(Width::W4))
            .map(|b| i32::from_be_bytes(flip_sign(b)))
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.exact(CellType::Int(Width::W8))
            .map(|b| i64::from_be_bytes(flip_sign(b)))
    }

    pub fn as_i128(&self) -> Option<i128> {
        self.exact(CellType::Int(Width::W16))
            .map(|b| i128::from_be_bytes(flip_sign(b)))
    }

    /// Two's complement big-endian; Rust has no `i256`.
    pub fn as_i256_be(&self) -> Option<[u8; 32]> {
        self.exact(CellType::Int(Width::W32)).map(flip_sign)
    }

    // Decimals return the unscaled integer; the scale is `Width::decimal_scale`.

    pub fn as_dec32_unscaled(&self) -> Option<i32> {
        self.exact(CellType::Decimal(Width::W4))
            .map(|b| i32::from_be_bytes(flip_sign(b)))
    }

    pub fn as_dec64_unscaled(&self) -> Option<i64> {
        self.exact(CellType::Decimal(Width::W8))
            .map(|b| i64::from_be_bytes(flip_sign(b)))
    }

    pub fn as_dec128_unscaled(&self) -> Option<i128> {
        self.exact(CellType::Decimal(Width::W16))
            .map(|b| i128::from_be_bytes(flip_sign(b)))
    }

    /// Two's complement big-endian.
    pub fn as_dec256_unscaled_be(&self) -> Option<[u8; 32]> {
        self.exact(CellType::Decimal(Width::W32)).map(flip_sign)
    }

    pub fn as_f32(&self) -> Option<f32> {
        self.exact(CellType::Float(FloatWidth::F32))
            .map(|b| f32::from_be_bytes(decode_float(b)))
    }

    pub fn as_f64(&self) -> Option<f64> {
        self.exact(CellType::Float(FloatWidth::F64))
            .map(|b| f64::from_be_bytes(decode_float(b)))
    }

    /// Days since the Unix epoch.
    pub fn as_date32(&self) -> Option<i32> {
        self.exact(CellType::Date32)
            .map(|b| i32::from_be_bytes(flip_sign(b)))
    }

    /// Microseconds since the Unix epoch.
    pub fn as_timestamp64(&self) -> Option<i64> {
        self.exact(CellType::Timestamp64)
            .map(|b| i64::from_be_bytes(flip_sign(b)))
    }
}
