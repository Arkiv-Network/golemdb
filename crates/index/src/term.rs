use golemdb_cells::{CellNameRef, CellType, CellValueParseError, CellValueRef, FloatWidth};
use golemdb_merkle::{Hash, HashProvider};
use std::{error::Error, fmt};

use crate::{Hi48, IndexError};

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
        check_str(ty, &ordered)?;
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
        Self::from_str_name(name, cell).map(Some)
    }

    /// The term `name = value` is indexed under, whatever the cell's kind:
    /// queries look a value up by name, type and value. The payload is already
    /// valid and order-encoded, so it is copied verbatim, as for `from_cell`.
    pub fn from_str_name(name: &str, value: CellValueRef<'_>) -> Result<Self, TermError> {
        check_str(value.cell_type(), value.value())?;
        let mut bytes = Self::prefix(name, value.cell_type())?;
        bytes.extend_from_slice(value.value());
        Ok(Self(bytes))
    }

    /// [`Self::for_value`] for a stored cell name. Names are raw bytes; one
    /// that is not UTF-8 is [`TermError::InvalidName`], like any name outside
    /// the grammar. Seal and queries both build terms through here.
    pub fn from_cell_name(
        name: CellNameRef<'_>,
        value: CellValueRef<'_>,
    ) -> Result<Self, TermError> {
        Self::from_str_name(name_str(name)?, value)
    }

    /// Check a stored cell name as [`Self::for_cell_name`] does, for a leaf
    /// with no value to build a term from.
    pub fn check_cell_name(name: CellNameRef<'_>) -> Result<(), TermError> {
        validate_name(name_str(name)?)
    }

    /// Prefix that confines range/prefix scans to one attribute name and type.
    pub fn prefix(name: &str, ty: CellType) -> Result<Vec<u8>, TermError> {
        let mut out = name_prefix(name)?;
        supported(ty)?;
        out.push(0x80 | ty.id());
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
        check_str(ty, value)?;
        Ok(Self(bytes.to_vec()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// The term's hash, which starts the index trie path of each of its
    /// containers (the term itself has no hash domain).
    pub fn routing_path(&self, hasher: &impl HashProvider) -> Hash {
        hasher.hash(&self.0)
    }

    /// The start of every container key of this term: the term, with a
    /// variable-width value ended by `0x00`, which no such value holds. No other
    /// term's key starts with it, and key order is term order.
    pub fn key(&self) -> Vec<u8> {
        key_bytes(&self.0)
    }

    /// The longest container key of a term whose name has `name_len` bytes
    /// and whose value has at most `value_len` bytes, or 32 if fixed-width:
    /// a string's terminator, then `hi48`, follow the value.
    pub const fn max_key_len(name_len: u64, value_len: u64) -> u64 {
        let string = value_len + 1;
        let value = if string > 32 { string } else { 32 };
        name_len + 2 + value + Hi48::BYTES as u64
    }

    /// Split a container key into its term and the bytes after the term's
    /// key, rejecting malformed and noncanonical terms.
    pub fn split_key(key: &[u8]) -> Result<(Self, &[u8]), TermError> {
        let bad = || TermError::InvalidEncoding;
        let split = key.iter().position(|&b| b == 0).ok_or_else(bad)?;
        let tag = *key.get(split + 1).ok_or_else(bad)?;
        let start = split + 2;
        let (raw, rest) = match value_width(tag) {
            Some(width) => {
                let end = start.checked_add(width).filter(|&end| end <= key.len());
                let end = end.ok_or_else(bad)?;
                (&key[..end], &key[end..])
            }
            None => {
                let end = key.get(start..).ok_or_else(bad)?;
                let end = start + end.iter().position(|&b| b == 0).ok_or_else(bad)?;
                (&key[..end], &key[end + 1..])
            }
        };
        Ok((Self::decode(raw)?, rest))
    }
}

/// A container's `Index` key: its term's key ([`IndexTerm::key`]), then its
/// [`Hi48`].
pub(crate) struct IndexKey(Vec<u8>);

impl IndexKey {
    pub(crate) fn new(term_key: &[u8], hi48: Hi48) -> Self {
        Self([term_key, &hi48.to_bytes()].concat())
    }

    /// Split a stored key into its term and its [`Hi48`], rejecting a
    /// malformed term or a suffix that is not a [`Hi48`].
    pub(crate) fn split(key: &[u8]) -> crate::Result<(IndexTerm, Hi48)> {
        let (term, suffix) = IndexTerm::split_key(key)?;
        Ok((term, Self::hi48(suffix)?))
    }

    /// The [`Hi48`] in `suffix`, the bytes after a term's key; any other
    /// length is a corrupt key.
    pub(crate) fn hi48(suffix: &[u8]) -> crate::Result<Hi48> {
        Hi48::from_slice(suffix).ok_or(IndexError::Corruption("malformed container key"))
    }
}

impl AsRef<[u8]> for IndexKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// The width of the value after a type tag, or `None` for a variable width
/// or an unknown tag.
fn value_width(tag: u8) -> Option<usize> {
    let ty = CellType::from_id(tag & 0x7f)
        .ok()
        .filter(|_| tag & 0x80 != 0)?;
    ty.width()
}

/// Key bytes for a complete term: a variable-width value is followed by
/// `0x00`. A string holds no `0x00`, so the key ends where its value does;
/// order is unchanged.
pub(crate) fn key_bytes(term: &[u8]) -> Vec<u8> {
    let tag = term
        .iter()
        .position(|&b| b == 0)
        .and_then(|split| term.get(split + 1));
    if tag.and_then(|&tag| value_width(tag)).is_some() {
        return term.to_vec();
    }
    [term, &[0]].concat()
}

/// A stored cell name as a term name: one that is not UTF-8 is
/// [`TermError::InvalidName`], like any name outside the grammar.
pub(crate) fn name_str(name: CellNameRef<'_>) -> Result<&str, TermError> {
    std::str::from_utf8(name.as_bytes()).map_err(|_| TermError::InvalidName)
}

/// The start of every term of `name`, whatever its type: the name, then the
/// `0x00` separator. A valid name holds no `0x00` byte, so no other name's
/// terms start with it, not even a longer name's (`ab` for `a`).
pub(crate) fn name_prefix(name: &str) -> Result<Vec<u8>, TermError> {
    validate_name(name)?;
    // Room for the type tag `prefix` appends.
    let mut out = Vec::with_capacity(name.len() + 2);
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    Ok(out)
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

/// A `str` value holding `0x00`, which ends a string in a key, has no term.
fn check_str(ty: CellType, value: &[u8]) -> Result<(), TermError> {
    if ty == CellType::Str && value.contains(&0) {
        return Err(TermError::Value(CellValueParseError::NulInAttribute));
    }
    Ok(())
}

/// The index holds the types a cell can be an attribute of, decided once by
/// [`CellType::can_be_attribute`], so the query operands' markers and the
/// index cannot disagree.
fn supported(ty: CellType) -> Result<(), TermError> {
    if ty.can_be_attribute() {
        Ok(())
    } else {
        Err(TermError::UnsupportedType(ty))
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
            let term = IndexTerm::from_str_name("Price", value.as_view()).expect("indexable");
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
            let term = IndexTerm::from_str_name("Price", value.as_view()).expect("indexable");
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
            let term = IndexTerm::from_str_name("Price", field.as_view()).expect("indexable");
            assert_eq!(
                IndexTerm::from_str_name("Price", attribute.as_view()).expect("indexable"),
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
        let price = |name| IndexTerm::from_str_name(name, CellValue::from_u32(7).as_view());
        assert_ne!(price("Price"), price("Price2"));
        assert_ne!(
            price("Price"),
            IndexTerm::from_str_name("Price", CellValue::from_i32(7).as_view())
        );
        assert!(price("$owner").is_ok());
    }

    #[test]
    fn for_value_rejects_invalid_names_and_variable_length_bytes() {
        let value = CellValue::from_u32(7);
        for name in ["", "$", "0bad", "#key", "@admin", "a b", "a\0b", "é"] {
            assert_eq!(
                IndexTerm::from_str_name(name, value.as_view()),
                Err(TermError::InvalidName),
                "{name:?}"
            );
        }
        assert_eq!(
            IndexTerm::from_str_name("Price", CellValue::from_bytes(b"x").as_view()),
            Err(TermError::UnsupportedType(CellType::Bytes))
        );
    }

    #[test]
    fn for_cell_name_decodes_the_name_then_is_for_value() {
        let value = CellValue::from_u32(7);
        for name in ["Price", "$owner", "1bad", "#key"] {
            assert_eq!(
                IndexTerm::from_cell_name(CellNameRef::raw(name.as_bytes()), value.as_view()),
                IndexTerm::from_str_name(name, value.as_view()),
                "{name:?}"
            );
        }
        assert_eq!(
            IndexTerm::from_cell_name(CellNameRef::raw(&[0xff]), value.as_view()),
            Err(TermError::InvalidName)
        );
    }

    #[test]
    fn check_cell_name_agrees_with_for_cell_name() {
        let value = CellValue::from_u32(7);
        for name in [&b"Price"[..], b"$owner", b"1bad", b"#key", b"", &[0xff]] {
            let name = CellNameRef::raw(name);
            assert_eq!(
                IndexTerm::check_cell_name(name),
                IndexTerm::from_cell_name(name, value.as_view()).map(drop),
                "{name:?}"
            );
        }
    }
}
