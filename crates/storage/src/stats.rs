/// A snapshot of a [`Store`](crate::Store)'s physical storage: the size of the
/// whole database, and counters for the current session, which start at zero
/// when the database is opened. Subtract the counters of two snapshots to get
/// the work between them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreStats {
    /// Bytes the whole database uses, including space freed for reuse.
    pub db_size: u64,
    /// Bytes written this session, possibly estimated.
    pub session_bytes_written: u64,
    /// Flushes to disk this session, such as `fsync`.
    pub session_syncs: u64,
}
