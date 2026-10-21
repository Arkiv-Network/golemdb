use std::ops::Range;

use golemdb_merkle::{COMMIT_BYTES, HASH_BYTES, Hash, HashProvider};
use golemdb_storage::{ReadCursor, ReadTransaction, Table, WriteTransaction};

use crate::{
    BranchError, CommitId, Result,
    roots::{ROOTS, Roots, put_roots},
};

const SUPERBLOCK: Table = Table("Superblock");
const HEAD_KEY: &[u8] = b"head";
// The head row, `commitNr: u64 ‖ stateRoot ‖ indexRoot` (design §4), by field.
const COMMIT: Range<usize> = 0..COMMIT_BYTES;
const STATE_ROOT: Range<usize> = COMMIT.end..COMMIT.end + HASH_BYTES;
const INDEX_ROOT: Range<usize> = STATE_ROOT.end..STATE_ROOT.end + HASH_BYTES;
const HEAD_BYTES: usize = INDEX_ROOT.end;

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
        commit_id: CommitId::new(u64::from_be_bytes(bytes[COMMIT].try_into().unwrap())),
        state_root: bytes[STATE_ROOT].try_into().unwrap(),
        index_root: bytes[INDEX_ROOT].try_into().unwrap(),
    })
}

/// Publish commit 0 into a store without a head; [`BranchError::HeadExists`]
/// otherwise.
pub fn init_head(
    tx: &mut impl WriteTransaction,
    roots: &Roots,
    hasher: &impl HashProvider,
) -> Result<Head> {
    if tx.get(SUPERBLOCK, HEAD_KEY)?.is_some() {
        return Err(BranchError::HeadExists);
    }
    publish(tx, CommitId::GENESIS, roots, hasher)
}

/// Publish the commit after `from` if `from` is still the head;
/// [`BranchError::Conflict`] otherwise.
pub fn advance_head(
    tx: &mut impl WriteTransaction,
    from: CommitId,
    roots: &Roots,
    hasher: &impl HashProvider,
) -> Result<Head> {
    if read_head(tx)? != from {
        return Err(BranchError::Conflict);
    }
    let commit = (from.get().checked_add(1)).ok_or(BranchError::CommitNumberExhausted)?;
    publish(tx, CommitId::new(commit), roots, hasher)
}

/// Stage the head for `commit_id` and its `Roots` row; the head's hashes come from `roots`.
/// Low level, for fixtures and tooling: it does not check the current head, only that
/// `commit_id` is newer than every commit in `Roots` ([`BranchError::CommitNotNewer`]).
/// Publishers use [`advance_head`] and [`init_head`].
pub fn write_head(
    tx: &mut impl WriteTransaction,
    commit_id: CommitId,
    roots: &Roots,
    hasher: &impl HashProvider,
) -> Result<Head> {
    // `Roots` is appended in commit order, so its last key is the latest commit.
    if let Some((key, _)) = tx.cursor(ROOTS, &u64::MAX.to_be_bytes())?.prev()? {
        let Ok(latest) = <[u8; COMMIT_BYTES]>::try_from(key.as_slice()) else {
            return Err(BranchError::InvalidRootsKey(key));
        };
        let latest = u64::from_be_bytes(latest);
        if commit_id.get() <= latest {
            return Err(BranchError::CommitNotNewer {
                commit: commit_id.get(),
                latest,
            });
        }
    }
    publish(tx, commit_id, roots, hasher)
}

/// The head and its `Roots` row, once the caller has checked `commit_id`.
fn publish(
    tx: &mut impl WriteTransaction,
    commit_id: CommitId,
    roots: &Roots,
    hasher: &impl HashProvider,
) -> Result<Head> {
    let head = Head {
        commit_id,
        state_root: roots.state.hash(hasher),
        index_root: roots.index.hash(hasher),
    };
    put_roots(tx, commit_id, roots)?;
    put_head(tx, &head)?;
    Ok(head)
}

/// The head row alone.
pub(crate) fn put_head(tx: &mut impl WriteTransaction, head: &Head) -> Result<()> {
    let mut bytes = [0; HEAD_BYTES];
    bytes[COMMIT].copy_from_slice(&head.commit_id.get().to_be_bytes());
    bytes[STATE_ROOT].copy_from_slice(&head.state_root);
    bytes[INDEX_ROOT].copy_from_slice(&head.index_root);
    tx.put(SUPERBLOCK, HEAD_KEY, &bytes)?;
    Ok(())
}
