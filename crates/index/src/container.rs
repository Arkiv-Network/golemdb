use roaring::RoaringBitmap;

use crate::{ContainerId, container_id::MAX_OFFSET, error::BitmapError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapContainer {
    container_id: ContainerId,
    values: RoaringBitmap,
}

impl BitmapContainer {
    /// Like [`Self::from_values`], but from a Roaring bitmap; fails if an
    /// offset doesn't fit in 16 bits.
    pub fn new(container_id: ContainerId, values: RoaringBitmap) -> Result<Self, BitmapError> {
        if let Some(value) = values.max()
            && value > MAX_OFFSET
        {
            return Err(BitmapError::OffsetOutOfRange { value });
        }

        Ok(Self {
            container_id,
            values,
        })
    }

    /// The container `container_id` with no offsets.
    pub fn empty(container_id: ContainerId) -> Self {
        Self::from_values(container_id, [])
    }

    /// The container `container_id` holding `values`, its record IDs' offsets.
    pub fn from_values(container_id: ContainerId, values: impl IntoIterator<Item = u16>) -> Self {
        Self {
            container_id,
            values: values.into_iter().map(u32::from).collect(),
        }
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
            .map(|offset| self.container_id.record_id(offset as u16))
    }

    /// Canonical Roaring bytes, without the [`ContainerId`]. Equal logical containers
    /// encode identically regardless of their mutation history.
    pub fn encode(&self) -> Result<Vec<u8>, BitmapError> {
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

    /// Decode only canonical Roaring bytes for the container `container_id`.
    /// Reject alternate representations and trailing bytes rather than silently
    /// normalizing data whose content hash may already be stored.
    pub fn decode(container_id: ContainerId, bytes: &[u8]) -> Result<Self, BitmapError> {
        let values = RoaringBitmap::deserialize_from(bytes).map_err(BitmapError::Decode)?;
        let container = Self::new(container_id, values)?;
        if container.encode()? != bytes {
            return Err(BitmapError::NonCanonicalEncoding);
        }
        Ok(container)
    }

    /// The container's ID: the top 48 bits its record IDs share.
    pub const fn container_id(&self) -> ContainerId {
        self.container_id
    }

    pub const fn bitmap(&self) -> &RoaringBitmap {
        &self.values
    }
}
