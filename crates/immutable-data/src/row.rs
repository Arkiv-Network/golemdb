use std::sync::Arc;

use golemdb_cells::{CellKind, CellType, CellValue, FloatWidth, Width};

use crate::schema::RowLayout;
use crate::{ImmutableDataError, Result};

/// Owned values in schema order on append and projection order on read.
/// Values are always fields; callers cannot mutate them without normalization.
/// Read rows carry shared column metadata for named access; append rows created
/// with `new` contain only ordered values. Equality includes layout metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImmutableDataRow {
    values: Vec<CellValue>,
    layout: Option<Arc<RowLayout>>,
}

impl ImmutableDataRow {
    /// Construct an unnamed append row, normalizing cell flags without changing
    /// types or payload bytes. Schema arity and types are validated separately by
    /// ImmutableDataSchema. Named access is available on decoded read rows.
    pub fn new(values: Vec<CellValue>) -> Self {
        Self {
            values: values
                .into_iter()
                .map(|value| {
                    value
                        .with_kind(CellKind::Field)
                        .expect("every cell type permits field values")
                })
                .collect(),
            layout: None,
        }
    }

    // The decoder has already validated types, arity and field flags.
    pub(crate) fn from_decoded(values: Vec<CellValue>, layout: Arc<RowLayout>) -> Self {
        Self {
            values,
            layout: Some(layout),
        }
    }

    pub fn values(&self) -> &[CellValue] {
        &self.values
    }

    pub fn into_values(self) -> Vec<CellValue> {
        self.values
    }

    /// Borrow payload bytes without cell type tags or indexable bits. These
    /// bytes retain the existing cell codecs, including signed-order encoding.
    pub fn payloads(&self) -> impl ExactSizeIterator<Item = &[u8]> + '_ {
        self.values.iter().map(CellValue::value)
    }

    /// Look up a selected column and convert it to an exact native type. Name
    /// lookup and type validation happen at runtime; conversion is generic.
    /// String and Vec<u8> results copy their data. For borrowed access use value.
    /// Never fetches missing columns or performs numeric coercion.
    pub fn get<T: FromImmutableDataValue>(&self, name: &str) -> Result<T> {
        let column = self.layout()?.column_index(name)?;
        let value = &self.values[column];
        if value.cell_type() != T::CELL_TYPE {
            return Err(ImmutableDataError::TypeMismatch {
                column,
                expected: T::CELL_TYPE,
                actual: value.cell_type(),
            });
        }
        T::from_value(value).ok_or(ImmutableDataError::NativeConversion {
            column,
            value_type: T::CELL_TYPE,
        })
    }

    /// Borrow a selected column's typed value, without copying its payload.
    /// An unknown name, an unselected column, or an unnamed append row is an error.
    pub fn value(&self, name: &str) -> Result<&CellValue> {
        let column = self.layout()?.column_index(name)?;
        Ok(&self.values[column])
    }

    /// Iterate over named values in projection order (schema order for a full
    /// row). Unnamed append rows return UnnamedRow; use values() for those rows.
    pub fn iter(&self) -> Result<impl ExactSizeIterator<Item = (&str, &CellValue)> + '_> {
        let layout = self.layout()?;
        Ok(self
            .values
            .iter()
            .enumerate()
            .map(move |(position, value)| (layout.column_name(position), value)))
    }

    fn layout(&self) -> Result<&RowLayout> {
        self.layout.as_deref().ok_or(ImmutableDataError::UnnamedRow)
    }
}

impl From<Vec<CellValue>> for ImmutableDataRow {
    fn from(values: Vec<CellValue>) -> Self {
        Self::new(values)
    }
}

/// A native column conversion used by ImmutableDataRow::get, which checks
/// CELL_TYPE first. Return None if a matching value cannot be converted.
///
/// Primitive implementations use existing cell accessors. Fixed byte arrays
/// mean bytesN, never 256-bit integers. Dates and decimals require explicit
/// application wrappers implementing this trait, or named CellValue accessors.
pub trait FromImmutableDataValue: Sized {
    const CELL_TYPE: CellType;
    fn from_value(value: &CellValue) -> Option<Self>;
}

macro_rules! native_values {
    ($($rust:ty => $cell:expr, $accessor:ident);+ $(;)?) => {
        $(impl FromImmutableDataValue for $rust {
            const CELL_TYPE: CellType = $cell;
            fn from_value(value: &CellValue) -> Option<Self> {
                value.$accessor()
            }
        })+
    };
}

native_values! {
    bool => CellType::Bool, as_bool;
    u32 => CellType::Uint(Width::W4), as_u32;
    u64 => CellType::Uint(Width::W8), as_u64;
    u128 => CellType::Uint(Width::W16), as_u128;
    i32 => CellType::Int(Width::W4), as_i32;
    i64 => CellType::Int(Width::W8), as_i64;
    i128 => CellType::Int(Width::W16), as_i128;
    f32 => CellType::Float(FloatWidth::F32), as_f32;
    f64 => CellType::Float(FloatWidth::F64), as_f64;
    [u8; 4] => CellType::FixedBytes(Width::W4), as_bytes4;
    [u8; 8] => CellType::FixedBytes(Width::W8), as_bytes8;
    [u8; 16] => CellType::FixedBytes(Width::W16), as_bytes16;
    [u8; 20] => CellType::Bytes20, as_bytes20;
    [u8; 32] => CellType::FixedBytes(Width::W32), as_bytes32;
}

impl FromImmutableDataValue for String {
    const CELL_TYPE: CellType = CellType::Str;
    fn from_value(value: &CellValue) -> Option<Self> {
        value.as_str().map(ToOwned::to_owned)
    }
}

impl FromImmutableDataValue for Vec<u8> {
    const CELL_TYPE: CellType = CellType::Bytes;
    fn from_value(value: &CellValue) -> Option<Self> {
        value.as_bytes().map(ToOwned::to_owned)
    }
}
