use crate::{CHUNK_BITS, CONTAINER_LEAF_DOMAIN, HI48_BYTES, MAX_OFFSET};
use golemdb_merkle::{Hash, HashProvider};
use roaring::RoaringBitmap;

use crate::{error::BitmapError, path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapContainer {
    path: u64,
    values: RoaringBitmap,
}

/// `H(0x04 || term_path || path || roaring)`: a container's leaf in the index
/// trie, from its term's routing path, its own path and its Roaring bytes.
pub(crate) fn leaf_hash(
    hasher: &impl HashProvider,
    term_path: &[u8],
    path: &[u8],
    roaring: &[u8],
) -> Hash {
    hasher.hash_parts(&[&[CONTAINER_LEAF_DOMAIN], term_path, path, roaring])
}

impl BitmapContainer {
    pub fn new(path: u64, values: RoaringBitmap) -> Result<Self, BitmapError> {
        if !path::is_valid_path(path) {
            return Err(BitmapError::PathOutOfRange { path });
        }
        if let Some(value) = values.max()
            && value > MAX_OFFSET
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
            .map(|offset| (self.path << CHUNK_BITS) | u64::from(offset))
    }

    /// Canonical Roaring bytes, without the path. Equal logical containers
    /// encode identically regardless of their mutation history.
    pub fn value_bytes(&self) -> Result<Vec<u8>, BitmapError> {
        if self.is_empty() {
            return Err(BitmapError::EmptyContainer);
        }
        let mut values = self.values.clone();
        values.remove_run_compression();
        values.optimize();
        let mut out = Vec::new();
        values
            .serialize_into(&mut out)
            .expect("writing to Vec cannot fail");
        Ok(out)
    }

    /// `path_be || canonical_roaring`.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, BitmapError> {
        Ok([&path::path_bytes(self.path)?[..], &self.value_bytes()?].concat())
    }

    /// `H(0x04 || term_path || path_be || canonical_roaring)`: the leaf of
    /// this container in the index trie, under the term routed by `term_path`.
    /// Empty working chunks have no leaf hash.
    pub fn leaf_hash(
        &self,
        term_path: &Hash,
        hasher: &impl HashProvider,
    ) -> Result<Hash, BitmapError> {
        let path = path::path_bytes(self.path)?;
        Ok(leaf_hash(hasher, term_path, &path, &self.value_bytes()?))
    }

    /// Decode only canonical Roaring bytes for the container at `path`.
    /// Reject alternate representations and trailing bytes rather than silently
    /// normalizing data whose content hash may already be stored.
    pub fn decode_values(path: u64, bytes: &[u8]) -> Result<Self, BitmapError> {
        let values = RoaringBitmap::deserialize_from(bytes).map_err(BitmapError::Decode)?;
        let container = Self::new(path, values)?;
        if container.value_bytes()? != bytes {
            return Err(BitmapError::NonCanonicalEncoding);
        }
        Ok(container)
    }

    /// Reverse [`Self::canonical_bytes`].
    pub fn decode(bytes: &[u8]) -> Result<Self, BitmapError> {
        let (hi, values) = bytes
            .split_first_chunk::<HI48_BYTES>()
            .ok_or(BitmapError::TruncatedPath)?;
        Self::decode_values(path::from_bytes(hi), values)
    }

    pub const fn path(&self) -> u64 {
        self.path
    }

    pub const fn bitmap(&self) -> &RoaringBitmap {
        &self.values
    }
}
