use std::fmt;

/// Bits of a record ID held by one container, as offsets below its
/// [`ContainerId`]: a `u16`, as [`ContainerId::split`] and
/// [`ContainerId::record_id`] take it.
pub(crate) const OFFSET_BITS: u32 = u16::BITS;

/// The largest offset within a container.
pub(crate) const MAX_OFFSET: u32 = u16::MAX as u32;

/// The top 48 bits a container's record IDs share, above their 16-bit offset:
/// its container ID, stored as six big-endian bytes, `hi48`. Splitting a
/// record ID or reading its six bytes always gives one; only
/// [`ContainerId::new`] checks a raw value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContainerId(u64);

impl ContainerId {
    /// Bits of a record ID above its offset.
    pub const BITS: u32 = 64 - OFFSET_BITS;

    /// Bytes of a [`ContainerId`], big-endian.
    pub const BYTES: usize = Self::BITS.div_ceil(8) as usize;

    /// The largest value.
    pub const MAX: Self = Self((1 << Self::BITS) - 1);

    /// `None` for values above [`ContainerId::MAX`].
    pub const fn new(value: u64) -> Option<Self> {
        if value > Self::MAX.0 {
            return None;
        }
        Some(Self(value))
    }

    /// A record ID's `ContainerId` and its offset within the container.
    pub const fn split(record_id: u64) -> (Self, u16) {
        (Self(record_id >> OFFSET_BITS), record_id as u16)
    }

    /// The record ID at `offset` in this container.
    pub const fn record_id(self, offset: u16) -> u64 {
        self.0 << OFFSET_BITS | offset as u64
    }

    /// Big-endian, as `Index` keys and index trie paths hold it.
    pub const fn to_bytes(self) -> [u8; Self::BYTES] {
        let [_, _, a, b, c, d, e, f] = self.0.to_be_bytes();
        [a, b, c, d, e, f]
    }

    /// Reverse [`ContainerId::to_bytes`]: any six bytes are a valid value.
    pub const fn from_bytes([a, b, c, d, e, f]: [u8; Self::BYTES]) -> Self {
        Self(u64::from_be_bytes([0, 0, a, b, c, d, e, f]))
    }

    /// `None` unless `bytes` is exactly a [`ContainerId`]'s six bytes.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        Some(Self::from_bytes(bytes.try_into().ok()?))
    }

    /// The value as a number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
