//! Batches of record operations for benchmarks, run through the public [`Api`].
//! A batch is what one branch applies before it is sealed and committed.
//!
//! The live records are numbered `first..next`. A batch deletes the oldest ones,
//! as expiry would, patches records spread across the rest, and creates new ones
//! at the end.
//!
//! The four indexed attributes differ only in how many records share each value,
//! and so each index term:
//!
//! - `one_of_5`: many records per term;
//! - `one_of_1000`: few records per term;
//! - `last_commit`: new terms in every commit;
//! - `unique`: a new term for every create.

use std::path::Path;

use golemdb_api::{
    Api, ApiError, BranchId, CellLimits, CellValue, Database, Genesis, HashAlgorithm, OpenConfig,
    OpenMode, OpenResult, RecordKey, RecordOp, op,
};

pub fn open(dir: &Path) -> OpenResult<Database> {
    let limits = CellLimits {
        max_cell_name_len: 32,
        max_str_len: 128,
        max_bytes_len: 128 * 1024,
    };
    let genesis = Genesis::new(HashAlgorithm::Keccak256, limits);
    Database::open(
        dir,
        &OpenConfig::new(genesis).with_mode(OpenMode::CreateNew),
    )
}

#[derive(Clone, Copy, Debug)]
pub struct Batch {
    pub creates: u64,
    pub patches: u64,
    pub deletes: u64,
}

impl Batch {
    pub const fn new(creates: u64, patches: u64, deletes: u64) -> Batch {
        Batch {
            creates,
            patches,
            deletes,
        }
    }
}

pub enum Op {
    Create(RecordOp<op::Create>),
    Patch(RecordOp<op::Patch>),
    Delete(RecordOp<op::Delete>),
}

/// Planning a batch leaves the workload unchanged. Once the batch commits, replace
/// the workload with the one [`Workload::plan`] returned.
#[derive(Clone, Copy, Default)]
pub struct Workload {
    first: u64,
    next: u64,
    commit: u64,
}

impl Workload {
    /// Returns the batch's operations and the workload after it commits. With too
    /// few live records, the batch deletes and patches fewer than requested.
    pub fn plan(self, batch: Batch) -> (Vec<Op>, Workload) {
        let commit = self.commit + 1;
        let mut ops = Vec::new();

        let first = (self.first + batch.deletes).min(self.next);
        for n in self.first..first {
            ops.push(Op::Delete(RecordOp::delete(key(n))));
        }

        let remaining = self.next - first;
        let patches = batch.patches.min(remaining);
        for i in 0..patches {
            let n = first + i * remaining / patches;
            ops.push(Op::Patch(patch(n, commit)));
        }

        let next = self.next + batch.creates;
        for n in self.next..next {
            ops.push(Op::Create(create(n, commit)));
        }

        (
            ops,
            Workload {
                first,
                next,
                commit,
            },
        )
    }
}

pub fn preload(db: &Database, records: u64) -> Result<Workload, ApiError> {
    let mut workload = Workload::default();
    while workload.next < records {
        let creates = (records - workload.next).min(5_000);
        let (ops, after) = workload.plan(Batch::new(creates, 0, 0));
        db.commit(execute(db, ops)?)?;
        workload = after;
    }
    Ok(workload)
}

/// Begins a branch and applies `ops`, with a checkpoint before each operation as
/// a host does per transaction. The caller seals, commits or discards the branch.
pub fn execute(db: &Database, ops: Vec<Op>) -> Result<BranchId, ApiError> {
    let branch = db.begin()?;
    for op in ops {
        db.checkpoint(branch)?;
        match op {
            Op::Create(op) => db.create(branch, op).map(drop)?,
            Op::Patch(op) => db.patch(branch, op)?,
            Op::Delete(op) => db.delete(branch, op)?,
        }
    }
    Ok(branch)
}

/// Reverses the bits of `n`, so that consecutive records get keys far apart, as
/// hashed keys would be.
pub fn key(n: u64) -> RecordKey {
    let mut key = [0; 32];
    key[..8].copy_from_slice(&n.reverse_bits().to_be_bytes());
    RecordKey(key)
}

fn create(n: u64, commit: u64) -> RecordOp<op::Create> {
    RecordOp::create(key(n))
        .attribute("one_of_5", CellValue::from_u64(n % 5))
        .unwrap()
        .attribute("one_of_1000", CellValue::from_u64(n % 1_000))
        .unwrap()
        .attribute("last_commit", CellValue::from_u64(commit))
        .unwrap()
        .attribute("unique", CellValue::from_u64(n))
        .unwrap()
        .field("payload", payload(commit))
        .unwrap()
}

fn patch(n: u64, commit: u64) -> RecordOp<op::Patch> {
    RecordOp::patch(key(n))
        // The new value usually differs, which moves the record between two large
        // index terms.
        .attribute("one_of_5", CellValue::from_u64((n + commit) % 5))
        .unwrap()
        .attribute("last_commit", CellValue::from_u64(commit))
        .unwrap()
        .field("payload", payload(commit))
        .unwrap()
}

/// Returns 1 KiB of bytes that depend on the commit, so that every patch writes
/// new content.
fn payload(commit: u64) -> CellValue {
    CellValue::from_bytes(&[commit as u8; 1024])
}
