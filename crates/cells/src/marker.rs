//! One zero-sized marker per cell type, and the sealed traits they
//! implement: [`CellMarker`] for every type, [`Indexable`] for every type
//! the index holds.
//!
//! A marker names a cell type where a Rust type cannot: an `i32` is the
//! value of an `i32`, a `dec32` or a `date32` cell, and a `[u8; 32]` that of
//! a `bytes32`, `u256`, `i256` or `dec256` cell. Code generic over one
//! marker builds values of exactly one cell type.
//!
//! ```
//! use golemdb_cells::CellValue;
//! use golemdb_cells::marker::{CellMarker, Date32, Dec32, I32};
//!
//! assert_eq!(Dec32::cell(123_450), CellValue::from_dec32_unscaled(123_450));
//! assert_ne!(Dec32::cell(7), I32::cell(7));
//! assert_ne!(Date32::cell(7), I32::cell(7));
//! ```
//!
//! Every marker is a [`CellMarker`], and every one but [`Bytes`] is
//! [`Indexable`]:
//!
//! ```
//! use golemdb_cells::CellValue;
//! use golemdb_cells::marker::{Bytes, CellMarker, Indexable, Str};
//!
//! fn indexable<T: Indexable>(_: T) {}
//!
//! indexable(Str);
//! assert_eq!(Bytes::cell(b"any length"), CellValue::from_bytes(b"any length"));
//! ```
//!
//! ```compile_fail,E0277
//! use golemdb_cells::marker::{Bytes, Indexable};
//!
//! fn indexable<T: Indexable>(_: T) {}
//!
//! indexable(Bytes);
//! ```

use crate::{CellType, CellValue, FloatWidth, NotNan32, NotNan64, Width};

/// A cell type, named at compile time. Implemented by this module's
/// markers only, one per type.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a cell type marker",
    label = "expected a marker such as `U32`, `Str` or `Dec32`",
    note = "the markers are in `golemdb_cells::marker`; values only known as `CellValue`s at run time go through the operand's runtime constructor: `EqValue::try_from`, `InValues::try_from` or `Bounds::try_new`"
)]
pub trait CellMarker: sealed::Sealed + Copy {
    /// The cell type this marker names.
    const CELL_TYPE: CellType;

    /// The Rust value a cell of this type is built from.
    type Value<'a>;

    /// `value` as a field of type [`Self::CELL_TYPE`], built as the matching
    /// `CellValue::from_*` constructor builds it. Cannot fail.
    fn cell(value: Self::Value<'_>) -> CellValue;
}

/// A cell type the index holds: every type but variable-length `bytes`.
///
/// The bound of every query operand: an `Eq`'s value, an `In`'s values, a
/// `Range`'s bounds.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an indexable cell type",
    label = "expected the marker of an indexable cell type, such as `U32`, `Str` or `Dec32`",
    note = "variable-length `bytes` has no index term, so its marker `Bytes` is not `Indexable`; every other type's marker is",
    note = "values only known as `CellValue`s at run time go through the operand's runtime constructor: `EqValue::try_from`, `InValues::try_from` or `Bounds::try_new`"
)]
pub trait Indexable: CellMarker {}

// The `do_not_recommend` impls keep the markers out of a failed bound's
// "other types that implement" list: the `on_unimplemented` notes say
// what to write instead.

/// `bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bool;

impl sealed::Sealed for Bool {}

#[diagnostic::do_not_recommend]
impl CellMarker for Bool {
    const CELL_TYPE: CellType = CellType::Bool;
    type Value<'a> = bool;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_bool(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Bool {}

/// `str`, from UTF-8 text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Str;

impl sealed::Sealed for Str {}

#[diagnostic::do_not_recommend]
impl CellMarker for Str {
    const CELL_TYPE: CellType = CellType::Str;
    type Value<'a> = &'a str;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_str(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Str {}

/// `bytes20`, the address width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bytes20;

impl sealed::Sealed for Bytes20 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Bytes20 {
    const CELL_TYPE: CellType = CellType::Bytes20;
    type Value<'a> = [u8; 20];

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_bytes20(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Bytes20 {}

/// `bytes4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bytes4;

impl sealed::Sealed for Bytes4 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Bytes4 {
    const CELL_TYPE: CellType = CellType::FixedBytes(Width::W4);
    type Value<'a> = [u8; 4];

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_bytes4(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Bytes4 {}

/// `bytes8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bytes8;

impl sealed::Sealed for Bytes8 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Bytes8 {
    const CELL_TYPE: CellType = CellType::FixedBytes(Width::W8);
    type Value<'a> = [u8; 8];

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_bytes8(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Bytes8 {}

/// `bytes16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bytes16;

impl sealed::Sealed for Bytes16 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Bytes16 {
    const CELL_TYPE: CellType = CellType::FixedBytes(Width::W16);
    type Value<'a> = [u8; 16];

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_bytes16(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Bytes16 {}

/// `bytes32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bytes32;

impl sealed::Sealed for Bytes32 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Bytes32 {
    const CELL_TYPE: CellType = CellType::FixedBytes(Width::W32);
    type Value<'a> = [u8; 32];

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_bytes32(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Bytes32 {}

/// `u32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct U32;

impl sealed::Sealed for U32 {}

#[diagnostic::do_not_recommend]
impl CellMarker for U32 {
    const CELL_TYPE: CellType = CellType::Uint(Width::W4);
    type Value<'a> = u32;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_u32(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for U32 {}

/// `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct U64;

impl sealed::Sealed for U64 {}

#[diagnostic::do_not_recommend]
impl CellMarker for U64 {
    const CELL_TYPE: CellType = CellType::Uint(Width::W8);
    type Value<'a> = u64;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_u64(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for U64 {}

/// `u128`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct U128;

impl sealed::Sealed for U128 {}

#[diagnostic::do_not_recommend]
impl CellMarker for U128 {
    const CELL_TYPE: CellType = CellType::Uint(Width::W16);
    type Value<'a> = u128;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_u128(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for U128 {}

/// `u256`, from its natural big-endian bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct U256;

impl sealed::Sealed for U256 {}

#[diagnostic::do_not_recommend]
impl CellMarker for U256 {
    const CELL_TYPE: CellType = CellType::Uint(Width::W32);
    type Value<'a> = [u8; 32];

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_u256_be(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for U256 {}

/// `i32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I32;

impl sealed::Sealed for I32 {}

#[diagnostic::do_not_recommend]
impl CellMarker for I32 {
    const CELL_TYPE: CellType = CellType::Int(Width::W4);
    type Value<'a> = i32;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_i32(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for I32 {}

/// `i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I64;

impl sealed::Sealed for I64 {}

#[diagnostic::do_not_recommend]
impl CellMarker for I64 {
    const CELL_TYPE: CellType = CellType::Int(Width::W8);
    type Value<'a> = i64;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_i64(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for I64 {}

/// `i128`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I128;

impl sealed::Sealed for I128 {}

#[diagnostic::do_not_recommend]
impl CellMarker for I128 {
    const CELL_TYPE: CellType = CellType::Int(Width::W16);
    type Value<'a> = i128;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_i128(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for I128 {}

/// `i256`, from its two's complement big-endian bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I256;

impl sealed::Sealed for I256 {}

#[diagnostic::do_not_recommend]
impl CellMarker for I256 {
    const CELL_TYPE: CellType = CellType::Int(Width::W32);
    type Value<'a> = [u8; 32];

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_i256_be(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for I256 {}

/// `dec32`, from the unscaled integer: `123_450` is 12.3450.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dec32;

impl sealed::Sealed for Dec32 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Dec32 {
    const CELL_TYPE: CellType = CellType::Decimal(Width::W4);
    type Value<'a> = i32;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_dec32_unscaled(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Dec32 {}

/// `dec64`, from the unscaled integer (six places).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dec64;

impl sealed::Sealed for Dec64 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Dec64 {
    const CELL_TYPE: CellType = CellType::Decimal(Width::W8);
    type Value<'a> = i64;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_dec64_unscaled(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Dec64 {}

/// `dec128`, from the unscaled integer (eighteen places).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dec128;

impl sealed::Sealed for Dec128 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Dec128 {
    const CELL_TYPE: CellType = CellType::Decimal(Width::W16);
    type Value<'a> = i128;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_dec128_unscaled(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Dec128 {}

/// `dec256`, from the unscaled two's complement big-endian bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dec256;

impl sealed::Sealed for Dec256 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Dec256 {
    const CELL_TYPE: CellType = CellType::Decimal(Width::W32);
    type Value<'a> = [u8; 32];

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_dec256_unscaled_be(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Dec256 {}

/// `f32`, from a float checked once not to be NaN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct F32;

impl sealed::Sealed for F32 {}

#[diagnostic::do_not_recommend]
impl CellMarker for F32 {
    const CELL_TYPE: CellType = CellType::Float(FloatWidth::F32);
    type Value<'a> = NotNan32;

    fn cell(value: Self::Value<'_>) -> CellValue {
        NotNan32::cell(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for F32 {}

/// `f64`, from a float checked once not to be NaN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct F64;

impl sealed::Sealed for F64 {}

#[diagnostic::do_not_recommend]
impl CellMarker for F64 {
    const CELL_TYPE: CellType = CellType::Float(FloatWidth::F64);
    type Value<'a> = NotNan64;

    fn cell(value: Self::Value<'_>) -> CellValue {
        NotNan64::cell(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for F64 {}

/// `date32`, from days since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Date32;

impl sealed::Sealed for Date32 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Date32 {
    const CELL_TYPE: CellType = CellType::Date32;
    type Value<'a> = i32;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_date32(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Date32 {}

/// `timestamp64`, from microseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp64;

impl sealed::Sealed for Timestamp64 {}

#[diagnostic::do_not_recommend]
impl CellMarker for Timestamp64 {
    const CELL_TYPE: CellType = CellType::Timestamp64;
    type Value<'a> = i64;

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_timestamp64(value)
    }
}

#[diagnostic::do_not_recommend]
impl Indexable for Timestamp64 {}

/// The types of the [`Indexable`] markers. Each entry needs its marker to
/// implement [`Indexable`], so a marker listed here without the impl does
/// not build.
const INDEXABLE: &[CellType] = &[
    indexable::<Bool>(),
    indexable::<Str>(),
    indexable::<Bytes20>(),
    indexable::<Bytes4>(),
    indexable::<Bytes8>(),
    indexable::<Bytes16>(),
    indexable::<Bytes32>(),
    indexable::<U32>(),
    indexable::<U64>(),
    indexable::<U128>(),
    indexable::<U256>(),
    indexable::<I32>(),
    indexable::<I64>(),
    indexable::<I128>(),
    indexable::<I256>(),
    indexable::<Dec32>(),
    indexable::<Dec64>(),
    indexable::<Dec128>(),
    indexable::<Dec256>(),
    indexable::<F32>(),
    indexable::<F64>(),
    indexable::<Date32>(),
    indexable::<Timestamp64>(),
];

/// `bytes`, variable length: a field only. It has no order, so no index
/// term, so it is not [`Indexable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bytes;

impl sealed::Sealed for Bytes {}

#[diagnostic::do_not_recommend]
impl CellMarker for Bytes {
    const CELL_TYPE: CellType = CellType::Bytes;
    type Value<'a> = &'a [u8];

    fn cell(value: Self::Value<'_>) -> CellValue {
        CellValue::from_bytes(value)
    }
}

/// The types of the markers that are not [`Indexable`]. A negative bound
/// cannot be written, so the module's `compile_fail` example proves that
/// [`Bytes`] does not implement it.
const FIELD_ONLY: &[CellType] = &[field_only::<Bytes>()];

/// `T`'s cell type, for a marker that implements [`Indexable`].
const fn indexable<T: Indexable>() -> CellType {
    T::CELL_TYPE
}

/// `T`'s cell type, for a marker listed as not [`Indexable`].
const fn field_only<T: CellMarker>() -> CellType {
    T::CELL_TYPE
}

// Checked as the crate builds, over the whole type-id space: a cell type
// added without its marker, or listed on the wrong side, is a build error
// rather than a failing test. `INDEXABLE`'s entries need the `Indexable`
// impl; that `Bytes` lacks it is the module's `compile_fail` example.
const _: () = check_markers();

/// Every cell type has exactly one marker, and that marker is
/// [`Indexable`] exactly when the type is.
const fn check_markers() {
    let mut id = 0;
    while id < 128 {
        if let Ok(ty) = CellType::from_id(id) {
            let indexable = count(INDEXABLE, id);
            let field_only = count(FIELD_ONLY, id);
            assert!(
                indexable + field_only == 1,
                "every cell type needs exactly one marker in `marker`"
            );
            assert!(
                (indexable == 1) == ty.can_be_attribute(),
                "a marker is `Indexable` exactly when its cell type is indexable"
            );
        }
        id += 1;
    }
}

/// How many of `types` have the type id `id`.
const fn count(types: &[CellType], id: u8) -> usize {
    match types {
        [] => 0,
        [first, rest @ ..] => (first.id() == id) as usize + count(rest, id),
    }
}

mod sealed {
    /// Only this module's markers: a marker free to pick a cell type per
    /// value would break the one-type rule the traits stand for.
    pub trait Sealed {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `T`'s cell is `expected`, of type `T::CELL_TYPE`.
    fn builds<T: CellMarker>(value: T::Value<'_>, expected: CellValue) {
        let cell = T::cell(value);
        assert_eq!(cell.cell_type(), T::CELL_TYPE);
        assert_eq!(cell, expected);
    }

    #[test]
    fn each_marker_builds_as_its_from_constructor() {
        let wide = [0x80; 32];
        let f32 = |value| NotNan32::new(value).expect("not NaN");
        let f64 = |value| NotNan64::new(value).expect("not NaN");
        builds::<Bool>(true, CellValue::from_bool(true));
        builds::<Str>("é", CellValue::from_str("é"));
        builds::<Bytes>(b"any length", CellValue::from_bytes(b"any length"));
        builds::<Bytes20>([1; 20], CellValue::from_bytes20([1; 20]));
        builds::<Bytes4>([1; 4], CellValue::from_bytes4([1; 4]));
        builds::<Bytes8>([1; 8], CellValue::from_bytes8([1; 8]));
        builds::<Bytes16>([1; 16], CellValue::from_bytes16([1; 16]));
        builds::<Bytes32>(wide, CellValue::from_bytes32(wide));
        builds::<U32>(7, CellValue::from_u32(7));
        builds::<U64>(7, CellValue::from_u64(7));
        builds::<U128>(7, CellValue::from_u128(7));
        builds::<U256>(wide, CellValue::from_u256_be(wide));
        builds::<I32>(-7, CellValue::from_i32(-7));
        builds::<I64>(-7, CellValue::from_i64(-7));
        builds::<I128>(-7, CellValue::from_i128(-7));
        builds::<I256>(wide, CellValue::from_i256_be(wide));
        builds::<Dec32>(-7, CellValue::from_dec32_unscaled(-7));
        builds::<Dec64>(-7, CellValue::from_dec64_unscaled(-7));
        builds::<Dec128>(-7, CellValue::from_dec128_unscaled(-7));
        builds::<Dec256>(wide, CellValue::from_dec256_unscaled_be(wide));
        builds::<F32>(f32(-1.5), CellValue::from_f32(-1.5).expect("not NaN"));
        builds::<F64>(f64(-1.5), CellValue::from_f64(-1.5).expect("not NaN"));
        builds::<Date32>(-7, CellValue::from_date32(-7));
        builds::<Timestamp64>(-7, CellValue::from_timestamp64(-7));
    }

    #[test]
    fn twenty_three_indexable_markers_and_one_field_only() {
        // The build-time check proves the mapping; this pins the counts the
        // proposal states.
        assert_eq!(INDEXABLE.len(), 23);
        assert_eq!(FIELD_ONLY, [CellType::Bytes]);
    }
}
