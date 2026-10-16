//! [`NotNan32`] and [`NotNan64`]: the values of `f32` and `f64` cells,
//! checked once, so a float cell builds without failing.

use std::{
    cmp::Ordering,
    hash::{Hash, Hasher},
};

use crate::{CellType, CellValue, CellValueParseError, FloatWidth, encode_float};

macro_rules! not_nan {
    ($(#[doc = $doc:literal])* $name:ident($float:ty), $width:expr) => {
        $(#[doc = $doc])*
        ///
        /// Never NaN, and zero is `+0.0`, as a stored float is, so `Eq`, `Ord`
        /// and `Hash` are lawful and follow the cell's order.
        #[derive(Debug, Clone, Copy, PartialEq)]
        pub struct $name($float);

        impl $name {
            /// `value`, unless it is NaN. `-0.0` becomes `+0.0`, and the
            /// infinities are kept, as the float cell constructors do.
            ///
            /// `const`, so a constant is checked when the crate builds.
            ///
            /// # Errors
            ///
            /// [`CellValueParseError::FloatNaN`] for any NaN.
            pub const fn new(value: $float) -> Result<Self, CellValueParseError> {
                if value.is_nan() {
                    Err(CellValueParseError::FloatNaN)
                } else if value == 0.0 {
                    Ok(Self(0.0))
                } else {
                    Ok(Self(value))
                }
            }

            /// The float: never NaN, never `-0.0`.
            pub const fn get(self) -> $float {
                self.0
            }

            /// The cell of this value, as a field.
            pub(crate) fn cell(self) -> CellValue {
                CellValue::field_from_typed(
                    CellType::Float($width),
                    &encode_float(self.0.to_be_bytes()),
                )
            }
        }

        impl Eq for $name {}

        impl Hash for $name {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.0.to_bits().hash(state);
            }
        }

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for $name {
            fn cmp(&self, other: &Self) -> Ordering {
                self.0.total_cmp(&other.0)
            }
        }
    };
}

not_nan! {
    /// The value of an `f32` cell.
    NotNan32(f32), FloatWidth::F32
}

not_nan! {
    /// The value of an `f64` cell.
    ///
    /// ```
    /// use golemdb_cells::NotNan64;
    ///
    /// const HALF: NotNan64 = match NotNan64::new(0.5) {
    ///     Ok(half) => half,
    ///     Err(_) => panic!("NaN"),
    /// };
    /// assert_eq!(HALF.get(), 0.5);
    /// assert!(NotNan64::new(f64::NAN).is_err());
    /// ```
    NotNan64(f64), FloatWidth::F64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_is_rejected() {
        assert_eq!(NotNan64::new(f64::NAN), Err(CellValueParseError::FloatNaN));
        assert_eq!(NotNan32::new(-f32::NAN), Err(CellValueParseError::FloatNaN));
    }

    #[test]
    fn negative_zero_is_zero_and_infinities_are_kept() {
        for (value, kept) in [
            (f64::NEG_INFINITY, f64::NEG_INFINITY),
            (-1.5, -1.5),
            (-0.0, 0.0),
            (0.0, 0.0),
            (f64::INFINITY, f64::INFINITY),
        ] {
            let not_nan = NotNan64::new(value).expect("not NaN");
            assert_eq!(not_nan.get().to_bits(), kept.to_bits(), "{value}");
            assert_eq!(not_nan.cell(), CellValue::from_f64(value).expect("not NaN"));
        }
        let zero = NotNan32::new(-0.0).expect("not NaN");
        assert_eq!(zero.get().to_bits(), 0.0_f32.to_bits());
        assert_eq!(zero.cell(), CellValue::from_f32(0.0).expect("not NaN"));
    }

    #[test]
    fn the_order_is_the_numeric_order() {
        let ascending = [f64::NEG_INFINITY, -1.5, 0.0, 1.5, f64::INFINITY]
            .map(|value| NotNan64::new(value).expect("not NaN"));
        assert!(ascending.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(NotNan64::new(-0.0), NotNan64::new(0.0));
    }
}
