use crate::INDEX_LEAF_DOMAIN;
use golemdb_cells::{CellType, CellValueParseError, CellValueRef, FloatWidth};
use golemdb_merkle::{Hash, HashProvider};
use std::{error::Error, fmt};

/// Canonical ordered index key: `name || 0x00 || attribute_tag || ordered_value`.
///
/// Strings have no length prefix. Signed numbers flip the sign bit; floats use
/// the sortable IEEE transform after rejecting NaNs and normalizing zero.
/// Ordering is meaningful within one name/type prefix. Name/value size policies
/// beyond the cell codec belong to the engine, which knows deployment limits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexTerm(Vec<u8>);

impl IndexTerm {
    /// Construct from native value bytes: signed numbers are two's-complement
    /// BE and floats are IEEE BE. This encodes their order once. For an already
    /// encoded cell payload, use `from_cell` instead.
    pub fn new(name: &str, ty: CellType, value: &[u8]) -> Result<Self, TermError> {
        let mut bytes = Self::prefix(name, ty)?;
        // Guard indexing/conversion below without interpreting native float
        // bits as the stored ordered representation expected by validate().
        if let Some(expected) = ty.width()
            && value.len() != expected
        {
            return Err(TermError::Value(CellValueParseError::LengthMismatch {
                ty,
                expected,
                actual: value.len(),
            }));
        }
        let mut ordered = value.to_vec();
        match ty {
            CellType::Int(_) | CellType::Decimal(_) | CellType::Date32 | CellType::Timestamp64 => {
                ordered[0] ^= 0x80;
            }
            CellType::Float(width) => {
                validate_float(&mut ordered, width)?;
                if ordered[0] & 0x80 != 0 {
                    for byte in &mut ordered {
                        *byte = !*byte;
                    }
                } else {
                    ordered[0] ^= 0x80;
                }
            }
            _ => {} // Supported by prefix(): unsigned, bool, string, fixed bytes.
        }
        ty.validate(&ordered).map_err(TermError::Value)?;
        bytes.extend_from_slice(&ordered);
        Ok(Self(bytes))
    }

    /// Fields, including reserved-record cells, produce no index term. Validate
    /// attribute names and types only after this check; system field keys may
    /// use names that are not user identifiers. Cell values are already valid
    /// and order-encoded: copy their payload verbatim without transforming it.
    pub fn from_cell(name: &str, cell: CellValueRef<'_>) -> Result<Option<Self>, TermError> {
        if !cell.is_indexable() {
            return Ok(None);
        }
        Self::for_value(name, cell).map(Some)
    }

    /// The term `name = value` is indexed under, whatever the cell's kind:
    /// queries look a value up by name, type and value. The payload is already
    /// valid and order-encoded, so it is copied verbatim, as for `from_cell`.
    pub fn for_value(name: &str, value: CellValueRef<'_>) -> Result<Self, TermError> {
        let mut bytes = Self::prefix(name, value.cell_type())?;
        bytes.extend_from_slice(value.value());
        Ok(Self(bytes))
    }

    /// Prefix that confines range/prefix scans to one attribute name and type.
    pub fn prefix(name: &str, ty: CellType) -> Result<Vec<u8>, TermError> {
        validate_name(name)?;
        supported(ty)?;
        let mut out = Vec::with_capacity(name.len() + 2);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&[0, 0x80 | ty.id()]);
        Ok(out)
    }

    /// Decode exactly one term, rejecting noncanonical forms rather than
    /// normalizing persisted bytes. The trailing string consumes all remaining
    /// bytes; fixed-width types must have exactly their declared width.
    pub fn decode(bytes: &[u8]) -> Result<Self, TermError> {
        let split = bytes
            .iter()
            .position(|&b| b == 0)
            .ok_or(TermError::InvalidEncoding)?;
        let name = std::str::from_utf8(&bytes[..split]).map_err(|_| TermError::InvalidName)?;
        validate_name(name)?;
        let (&tag, value) = bytes[split + 1..]
            .split_first()
            .ok_or(TermError::InvalidEncoding)?;
        if tag & 0x80 == 0 {
            return Err(TermError::InvalidEncoding);
        }
        let ty = CellType::from_id(tag & 0x7f).map_err(TermError::Value)?;
        supported(ty)?;
        // Stored terms and cells share the same canonical ordered payload.
        // Validate it directly, without decoding/re-encoding native numbers.
        ty.validate(value).map_err(TermError::Value)?;
        Ok(Self(bytes.to_vec()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Full 32-byte IndexTrie routing path (the term itself has no hash domain).
    pub fn routing_path(&self, hasher: &impl HashProvider) -> Hash {
        hasher.hash(&self.0)
    }

    /// `H(0x02 || complete_term_path || bitmap_root)`.
    pub fn leaf_hash(&self, bitmap_root: &Hash, hasher: &impl HashProvider) -> Hash {
        hasher.hash_parts(&[
            &[INDEX_LEAF_DOMAIN],
            &self.routing_path(hasher),
            bitmap_root,
        ])
    }
}

fn validate_name(name: &str) -> Result<(), TermError> {
    let bytes = name.as_bytes();
    let bytes = bytes.strip_prefix(b"$").unwrap_or(bytes);
    let Some((&first, rest)) = bytes.split_first() else {
        return Err(TermError::InvalidName);
    };
    if !first.is_ascii_alphabetic()
        || !rest
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(b))
    {
        return Err(TermError::InvalidName);
    }
    Ok(())
}

fn supported(ty: CellType) -> Result<(), TermError> {
    match ty {
        CellType::Bool
        | CellType::Str
        | CellType::Bytes20
        | CellType::FixedBytes(_)
        | CellType::Uint(_)
        | CellType::Int(_)
        | CellType::Decimal(_)
        | CellType::Date32
        | CellType::Timestamp64
        | CellType::Float(_) => Ok(()),
        _ => Err(TermError::UnsupportedType(ty)),
    }
}

// Classify using bits, avoiding floating-point arithmetic and preserving all
// nonzero finite/infinite encodings exactly, including subnormals.
fn validate_float(bytes: &mut [u8], width: FloatWidth) -> Result<(), TermError> {
    let (bits, sign, exponent, fraction) = match width {
        FloatWidth::F32 => (
            u32::from_be_bytes(bytes.try_into().expect("validated width")) as u64,
            0x8000_0000,
            0x7f80_0000,
            0x007f_ffff,
        ),
        FloatWidth::F64 => (
            u64::from_be_bytes(bytes.try_into().expect("validated width")),
            0x8000_0000_0000_0000,
            0x7ff0_0000_0000_0000,
            0x000f_ffff_ffff_ffff,
        ),
    };
    if bits & exponent == exponent && bits & fraction != 0 {
        return Err(TermError::NaN);
    }
    if bits & !sign == 0 {
        bytes.fill(0);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermError {
    InvalidName,
    UnsupportedType(CellType),
    Value(CellValueParseError),
    NaN,
    InvalidEncoding,
}

impl fmt::Display for TermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => f.write_str("invalid attribute name"),
            Self::UnsupportedType(ty) => write!(f, "no ordered index codec for {ty:?}"),
            Self::Value(error) => write!(f, "invalid index value: {error}"),
            Self::NaN => f.write_str("NaN cannot be indexed"),
            Self::InvalidEncoding => f.write_str("invalid or noncanonical index term"),
        }
    }
}

impl Error for TermError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Value(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{CellKind, CellValue};

    use super::*;

    /// One value of every indexable type, with the native big-endian bytes
    /// `IndexTerm::new` takes for it.
    fn values() -> Vec<(CellValue, Vec<u8>)> {
        let mut i256 = [0xff; 32];
        i256[31] = 0xfe; // -2
        let float = |value: f64| CellValue::from_f64(value).expect("not NaN");
        vec![
            (CellValue::from_bool(true), vec![1]),
            (CellValue::from_str("blue"), b"blue".to_vec()),
            (CellValue::from_str(""), vec![]),
            (CellValue::from_bytes4([1, 2, 3, 4]), vec![1, 2, 3, 4]),
            (CellValue::from_bytes8([8; 8]), vec![8; 8]),
            (CellValue::from_bytes16([16; 16]), vec![16; 16]),
            (CellValue::from_bytes20([20; 20]), vec![20; 20]),
            (CellValue::from_bytes32([32; 32]), vec![32; 32]),
            (CellValue::from_u32(7), 7u32.to_be_bytes().to_vec()),
            (
                CellValue::from_u64(u64::MAX),
                u64::MAX.to_be_bytes().to_vec(),
            ),
            (CellValue::from_u128(1), 1u128.to_be_bytes().to_vec()),
            (CellValue::from_u256_be([0xff; 32]), vec![0xff; 32]),
            (CellValue::from_i32(-7), (-7i32).to_be_bytes().to_vec()),
            (
                CellValue::from_i64(i64::MIN),
                i64::MIN.to_be_bytes().to_vec(),
            ),
            (
                CellValue::from_i128(i128::MAX),
                i128::MAX.to_be_bytes().to_vec(),
            ),
            (CellValue::from_i256_be(i256), i256.to_vec()),
            (
                CellValue::from_dec32_unscaled(-12_345),
                (-12_345i32).to_be_bytes().to_vec(),
            ),
            (
                CellValue::from_dec64_unscaled(1),
                1i64.to_be_bytes().to_vec(),
            ),
            (
                CellValue::from_dec128_unscaled(-1),
                (-1i128).to_be_bytes().to_vec(),
            ),
            (CellValue::from_dec256_unscaled_be(i256), i256.to_vec()),
            (
                CellValue::from_f32(-1.5).expect("not NaN"),
                (-1.5f32).to_be_bytes().to_vec(),
            ),
            (float(f64::INFINITY), f64::INFINITY.to_be_bytes().to_vec()),
            // Both constructions normalize negative zero.
            (float(-0.0), (-0.0f64).to_be_bytes().to_vec()),
            (CellValue::from_date32(-1), (-1i32).to_be_bytes().to_vec()),
            (
                CellValue::from_timestamp64(i64::MIN),
                i64::MIN.to_be_bytes().to_vec(),
            ),
        ]
    }

    #[test]
    fn for_value_appends_the_stored_payload_to_the_prefix() {
        for (value, _) in values() {
            let term = IndexTerm::for_value("Price", value.as_view()).expect("indexable");
            let mut expected = IndexTerm::prefix("Price", value.cell_type()).expect("indexable");
            expected.extend_from_slice(value.value());
            assert_eq!(term.as_bytes(), expected, "{value:?}");
            assert_eq!(
                IndexTerm::decode(term.as_bytes()).expect("canonical"),
                term,
                "{value:?}"
            );
        }
    }

    #[test]
    fn for_value_agrees_with_native_construction() {
        for (value, native) in values() {
            let term = IndexTerm::for_value("Price", value.as_view()).expect("indexable");
            let from_native = IndexTerm::new("Price", value.cell_type(), &native).expect("valid");
            assert_eq!(term, from_native, "{value:?}");
        }
    }

    #[test]
    fn for_value_ignores_the_cell_kind() {
        for (field, _) in values() {
            let attribute = field
                .clone()
                .with_kind(CellKind::Attribute)
                .expect("indexable type");
            let term = IndexTerm::for_value("Price", field.as_view()).expect("indexable");
            assert_eq!(
                IndexTerm::for_value("Price", attribute.as_view()).expect("indexable"),
                term
            );
            // from_cell is for_value for attributes, and nothing for fields.
            assert_eq!(
                IndexTerm::from_cell("Price", attribute.as_view()).expect("indexable"),
                Some(term)
            );
            assert_eq!(
                IndexTerm::from_cell("Price", field.as_view()).expect("indexable"),
                None
            );
        }
    }

    #[test]
    fn for_value_keeps_names_and_types_apart() {
        let price = |name| IndexTerm::for_value(name, CellValue::from_u32(7).as_view());
        assert_ne!(price("Price"), price("Price2"));
        assert_ne!(
            price("Price"),
            IndexTerm::for_value("Price", CellValue::from_i32(7).as_view())
        );
        assert!(price("$owner").is_ok());
    }

    #[test]
    fn for_value_rejects_invalid_names_and_variable_length_bytes() {
        let value = CellValue::from_u32(7);
        for name in ["", "$", "0bad", "#key", "@admin", "a b", "a\0b", "é"] {
            assert_eq!(
                IndexTerm::for_value(name, value.as_view()),
                Err(TermError::InvalidName),
                "{name:?}"
            );
        }
        assert_eq!(
            IndexTerm::for_value("Price", CellValue::from_bytes(b"x").as_view()),
            Err(TermError::UnsupportedType(CellType::Bytes))
        );
    }
}
