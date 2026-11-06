use std::fmt;

use crate::OFFSET_BITS;

/// The top 48 bits a container's record IDs share, above their 16-bit offset:
/// the container's key in the index. Splitting a record ID or reading its six
/// bytes always gives one; only [`Hi48::new`] checks a raw value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hi48(u64);

impl Hi48 {
    /// Bits of a record ID above its offset.
    pub const BITS: u32 = 64 - OFFSET_BITS;

    /// Bytes of a [`Hi48`], big-endian.
    pub const BYTES: usize = Self::BITS.div_ceil(8) as usize;

    /// The largest value.
    pub const MAX: Self = Self((1 << Self::BITS) - 1);

    /// `None` for values above [`Hi48::MAX`].
    pub const fn new(value: u64) -> Option<Self> {
        if value > Self::MAX.0 {
            return None;
        }
        Some(Self(value))
    }

    /// A record ID's `Hi48` and its offset within the container.
    pub const fn split(record_id: u64) -> (Self, u16) {
        (Self(record_id >> OFFSET_BITS), record_id as u16)
    }

    /// The record ID at `offset` in the container this keys.
    pub const fn record_id(self, offset: u16) -> u64 {
        self.0 << OFFSET_BITS | offset as u64
    }

    /// Big-endian, as `Index` keys and index trie paths hold it.
    pub const fn to_bytes(self) -> [u8; Self::BYTES] {
        let [_, _, a, b, c, d, e, f] = self.0.to_be_bytes();
        [a, b, c, d, e, f]
    }

    /// Reverse [`Hi48::to_bytes`]: any six bytes are a valid value.
    pub const fn from_bytes([a, b, c, d, e, f]: [u8; Self::BYTES]) -> Self {
        Self(u64::from_be_bytes([0, 0, a, b, c, d, e, f]))
    }

    /// `None` unless `bytes` is exactly a [`Hi48`]'s six bytes.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        Some(Self::from_bytes(bytes.try_into().ok()?))
    }

    /// The value as a number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Hi48 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
