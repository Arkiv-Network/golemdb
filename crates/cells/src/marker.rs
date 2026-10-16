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

use crate::{CellType, CellValue, FloatWidth, NotNan32, NotNan64, Width};

/// A cell type, named at compile time. Implemented by this module's
/// markers only, one per type.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a cell type marker",
    label = "expected a marker such as `U32`, `Str` or `Dec32`",
    note = "the markers are in `golemdb_cells::marker`; values only known as `CellValue`s at run time go through the operand's `try_from`"
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
/// The bound of every query operand: an `In`'s values, a `Range`'s bounds.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an indexable cell type",
    label = "expected the marker of an indexable cell type, such as `U32`, `Str` or `Dec32`",
    note = "variable-length `bytes` has no index term, so its marker `Bytes` is not `Indexable`; every other type's marker is",
    note = "values only known as `CellValue`s at run time go through the operand's `try_from`"
)]
pub trait Indexable: CellMarker {}

/// A marker: a unit struct and its [`CellMarker`] impl.
macro_rules! marker {
    ($(#[doc = $doc:literal])* $marker:ident($value:ty) = $cell_type:expr, $build:expr) => {
        $(#[doc = $doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $marker;

        impl sealed::Sealed for $marker {}

        // Not listed as "other types that implement" in a failed bound: the
        // `on_unimplemented` notes say what to write instead.
        #[diagnostic::do_not_recommend]
        impl CellMarker for $marker {
            const CELL_TYPE: CellType = $cell_type;
            type Value<'a> = $value;

            fn cell(value: Self::Value<'_>) -> CellValue {
                $build(value)
            }
        }
    };
}

/// The indexable markers, and the list the build-time check reads.
macro_rules! indexable {
    ($($(#[doc = $doc:literal])* $marker:ident($value:ty) = $cell_type:expr, $build:expr;)*) => {
        $(
            marker!($(#[doc = $doc])* $marker($value) = $cell_type, $build);

            #[diagnostic::do_not_recommend]
            impl Indexable for $marker {}
        )*

        /// The types of the [`Indexable`] markers.
        const INDEXABLE: &[CellType] = &[$(<$marker as CellMarker>::CELL_TYPE),*];
    };
}

indexable! {
    /// `bool`.
    Bool(bool) = CellType::Bool, CellValue::from_bool;
    /// `str`, from UTF-8 text.
    Str(&'a str) = CellType::Str, CellValue::from_str;
    /// `bytes20`, the address width.
    Bytes20([u8; 20]) = CellType::Bytes20, CellValue::from_bytes20;
    /// `bytes4`.
    Bytes4([u8; 4]) = CellType::FixedBytes(Width::W4), CellValue::from_bytes4;
    /// `bytes8`.
    Bytes8([u8; 8]) = CellType::FixedBytes(Width::W8), CellValue::from_bytes8;
    /// `bytes16`.
    Bytes16([u8; 16]) = CellType::FixedBytes(Width::W16), CellValue::from_bytes16;
    /// `bytes32`.
    Bytes32([u8; 32]) = CellType::FixedBytes(Width::W32), CellValue::from_bytes32;
    /// `u32`.
    U32(u32) = CellType::Uint(Width::W4), CellValue::from_u32;
    /// `u64`.
    U64(u64) = CellType::Uint(Width::W8), CellValue::from_u64;
    /// `u128`.
    U128(u128) = CellType::Uint(Width::W16), CellValue::from_u128;
    /// `u256`, from its natural big-endian bytes.
    U256([u8; 32]) = CellType::Uint(Width::W32), CellValue::from_u256_be;
    /// `i32`.
    I32(i32) = CellType::Int(Width::W4), CellValue::from_i32;
    /// `i64`.
    I64(i64) = CellType::Int(Width::W8), CellValue::from_i64;
    /// `i128`.
    I128(i128) = CellType::Int(Width::W16), CellValue::from_i128;
    /// `i256`, from its two's complement big-endian bytes.
    I256([u8; 32]) = CellType::Int(Width::W32), CellValue::from_i256_be;
    /// `dec32`, from the unscaled integer: `123_450` is 12.3450.
    Dec32(i32) = CellType::Decimal(Width::W4), CellValue::from_dec32_unscaled;
    /// `dec64`, from the unscaled integer (six places).
    Dec64(i64) = CellType::Decimal(Width::W8), CellValue::from_dec64_unscaled;
    /// `dec128`, from the unscaled integer (eighteen places).
    Dec128(i128) = CellType::Decimal(Width::W16), CellValue::from_dec128_unscaled;
    /// `dec256`, from the unscaled two's complement big-endian bytes.
    Dec256([u8; 32]) = CellType::Decimal(Width::W32), CellValue::from_dec256_unscaled_be;
    /// `f32`, from a float checked once not to be NaN.
    F32(NotNan32) = CellType::Float(FloatWidth::F32), NotNan32::cell;
    /// `f64`, from a float checked once not to be NaN.
    F64(NotNan64) = CellType::Float(FloatWidth::F64), NotNan64::cell;
    /// `date32`, from days since the Unix epoch.
    Date32(i32) = CellType::Date32, CellValue::from_date32;
    /// `timestamp64`, from microseconds since the Unix epoch.
    Timestamp64(i64) = CellType::Timestamp64, CellValue::from_timestamp64;
}

marker! {
    /// `bytes`, variable length: a field only. It has no order, so no index
    /// term, so it is not [`Indexable`].
    Bytes(&'a [u8]) = CellType::Bytes, CellValue::from_bytes
}

/// The types of the markers that are not [`Indexable`].
const FIELD_ONLY: &[CellType] = &[<Bytes as CellMarker>::CELL_TYPE];

// Checked as the crate builds, over the whole type-id space: a cell type
// added without its marker, or with a marker on the wrong side, is a build
// error rather than a failing test.
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
                (indexable == 1) == ty.is_indexable(),
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
