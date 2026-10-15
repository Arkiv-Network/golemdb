/// A snapshot of a [`Store`](crate::Store)'s physical storage. Subtract the
/// counters of two snapshots to get the work between them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreStats {
    /// Bytes in use now, including space freed for reuse.
    pub size: u64,
    /// Bytes written so far, possibly estimated.
    pub bytes_written: u64,
    /// Flushes to disk so far, such as `fsync`.
    pub syncs: u64,
}
