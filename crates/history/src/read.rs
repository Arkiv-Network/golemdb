use golemdb_storage::Table;

/// Upcoming read operation. Callbacks can inspect or reject it before it runs.
/// Commit-set processing is reported once, before the first bitmap
/// copy/decode, even if a scan buffers the row for later successor selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOperation {
    Lookup(Table),
    /// Includes attempted exhaustion/out-of-range probes.
    Advance(Table),
    /// Encoded value bytes only, excluding the history key.
    CommitSet {
        encoded_bytes: u64,
    },
}
