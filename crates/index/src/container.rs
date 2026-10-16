use crate::BITMAP_LEAF_DOMAIN;
use golemdb_merkle::{Hash, HashProvider};
use roaring::RoaringBitmap;

use crate::{error::BitmapError, path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapContainer {
    path: u64,
    values: RoaringBitmap,
}

impl BitmapContainer {
    pub fn new(path: u64, values: RoaringBitmap) -> Result<Self, BitmapError> {
        if !path::is_valid_path(path) {
            return Err(BitmapError::PathOutOfRange { path });
        }
        if let Some(value) = values.max()
            && value > u16::MAX as u32
        {
            return Err(BitmapError::OffsetOutOfRange { value });
        }

        Ok(Self { path, values })
    }

    pub fn empty(path: u64) -> Result<Self, BitmapError> {
        Self::new(path, RoaringBitmap::new())
    }

    pub fn from_values(
        path: u64,
        values: impl IntoIterator<Item = u16>,
    ) -> Result<Self, BitmapError> {
        Self::new(path, values.into_iter().map(u32::from).collect())
    }

    pub fn insert(&mut self, offset: u16) -> bool {
        self.values.insert(offset.into())
    }

    pub fn remove(&mut self, offset: u16) -> bool {
        self.values.remove(offset.into())
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.values.len()
    }

    /// Complete record IDs, not local offsets.
    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.values
            .iter()
            .map(|offset| (self.path << 16) | u64::from(offset))
    }

    /// Append `hi48_be || canonical_roaring` to the output. Equal logical
    /// containers encode identically regardless of their mutation history.
    /// Call once per finalized dirty chunk, not after each offset change.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), BitmapError> {
        if self.is_empty() {
            return Err(BitmapError::EmptyContainer);
        }
        let mut values = self.values.clone();
        values.remove_run_compression();
        values.optimize();
        out.extend_from_slice(&path::path_bytes(self.path)?);
        values
            .serialize_into(out)
            .expect("writing to Vec cannot fail");
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, BitmapError> {
        let mut out = Vec::new();
        self.encode_into(&mut out)?;
        Ok(out)
    }

    /// `H(0x04 || hi48_be || canonical_roaring)`. Empty working chunks have no
    /// leaf hash.
    pub fn leaf_hash(&self, hasher: &impl HashProvider) -> Result<Hash, BitmapError> {
        Ok(hasher.hash_parts(&[&[BITMAP_LEAF_DOMAIN], &self.canonical_bytes()?]))
    }

    /// Decode only canonical persisted bytes, including the full path.
    /// Reject alternate representations and trailing bytes rather than silently
    /// normalizing data whose content hash may already be stored.
    pub fn decode(bytes: &[u8]) -> Result<Self, BitmapError> {
        let hi = bytes.get(..6).ok_or(BitmapError::TruncatedPath)?;
        let mut path_bytes = [0u8; 8];
        path_bytes[2..].copy_from_slice(hi);
        let values = RoaringBitmap::deserialize_from(&bytes[6..]).map_err(BitmapError::Decode)?;
        let container = Self::new(u64::from_be_bytes(path_bytes), values)?;
        if container.canonical_bytes()? != bytes {
            return Err(BitmapError::NonCanonicalEncoding);
        }
        Ok(container)
    }

    pub const fn path(&self) -> u64 {
        self.path
    }

    pub const fn bitmap(&self) -> &RoaringBitmap {
        &self.values
    }
}
