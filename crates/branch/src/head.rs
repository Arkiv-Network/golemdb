use golemdb_merkle::Hash;
use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

use crate::{BranchError, CommitId, Result};

const SUPERBLOCK: Table = Table("Superblock");
const HEAD_KEY: &[u8] = b"head";
const HEAD_BYTES: usize = 8 + 32 + 32;

/// Durable head metadata shared by genesis opening and branch publication.
/// The roots' locators live in the `Roots` row for `commit_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub commit_id: CommitId,
    pub state_root: Hash,
    pub index_root: Hash,
}

/// Read the head commit ID from a caller-owned snapshot. To read its committed
/// cells consistently, use this same transaction with the cell storage reader.
pub fn read_head(tx: &impl ReadTransaction) -> Result<CommitId> {
    Ok(read_head_state(tx)?.commit_id)
}

/// Decode the design's fixed-width head row from one snapshot.
pub fn read_head_state(tx: &impl ReadTransaction) -> Result<Head> {
    let bytes = tx
        .get(SUPERBLOCK, HEAD_KEY)?
        .ok_or(BranchError::MissingHead)?;
    if bytes.len() != HEAD_BYTES {
        return Err(BranchError::InvalidHead {
            actual: bytes.len(),
        });
    }
    Ok(Head {
        commit_id: CommitId::new(u64::from_be_bytes(bytes[..8].try_into().unwrap())),
        state_root: bytes[8..40].try_into().unwrap(),
        index_root: bytes[40..72].try_into().unwrap(),
    })
}

/// Stage head metadata in the same transaction as its cells and trie writes.
/// This is a trusted engine operation, not a public data API operation.
pub fn write_head(tx: &mut impl WriteTransaction, head: &Head) -> Result<()> {
    let mut bytes = [0; HEAD_BYTES];
    bytes[..8].copy_from_slice(&head.commit_id.get().to_be_bytes());
    bytes[8..40].copy_from_slice(&head.state_root);
    bytes[40..].copy_from_slice(&head.index_root);
    tx.put(SUPERBLOCK, HEAD_KEY, &bytes)?;
    Ok(())
}
