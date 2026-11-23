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

use golemdb_api::{Api, ApiError, BranchId, CellValue, Database, RecordOp, op};

use crate::common::{BUDGET, key};

/// How many records one batch creates, patches and deletes.
#[derive(Clone, Copy)]
pub struct Batch {
    pub creates: u64,
    pub patches: u64,
    pub deletes: u64,
}

/// Which records exist. [`Workload::commit`] moves it past each committed batch.
#[derive(Clone, Copy, Default)]
pub struct Workload {
    /// Oldest live record; all below it are deleted.
    first: u64,
    /// Next record to create; live records are `first..next`.
    next: u64,
    /// Batches committed so far.
    commits: u64,
}

impl Workload {
    /// The workload after `batch` commits.
    fn after(&self, batch: Batch) -> Workload {
        let live = self.next - self.first;
        assert!(
            batch.deletes + batch.patches <= live,
            "too few live records"
        );
        Workload {
            first: self.first + batch.deletes,
            next: self.next + batch.creates,
            commits: self.commits + 1,
        }
    }

    /// Applies `batch` to a new branch, with a checkpoint before each operation
    /// as a host does per transaction.
    pub fn apply(&self, db: &Database, batch: Batch) -> Result<BranchId, ApiError> {
        let after = self.after(batch);
        let live = self.next - after.first;
        let branch = db.begin()?;
        for n in self.first..after.first {
            db.checkpoint(branch)?;
            db.delete(branch, RecordOp::delete(key(n), BUDGET))
                .into_result()?;
        }
        for i in 0..batch.patches {
            db.checkpoint(branch)?;
            let n = after.first + i * live / batch.patches;
            db.patch(branch, patch(n, after.commits)).into_result()?;
        }
        for n in self.next..after.next {
            db.checkpoint(branch)?;
            db.create(branch, create(n, after.commits)).into_result()?;
        }
        Ok(branch)
    }

    /// Commits `batch`, then moves the workload past it.
    pub fn commit(&mut self, db: &Database, batch: Batch) -> Result<(), ApiError> {
        db.commit(self.apply(db, batch)?)?;
        *self = self.after(batch);
        Ok(())
    }
}

fn create(n: u64, commit: u64) -> RecordOp<op::Create> {
    RecordOp::create(key(n), BUDGET)
        .attribute("one_of_5", CellValue::from_u64(n % 5))
        .attribute("one_of_1000", CellValue::from_u64(n % 1_000))
        .attribute("last_commit", CellValue::from_u64(commit))
        .attribute("unique", CellValue::from_u64(n))
        .field("payload", payload(commit))
}

fn patch(n: u64, commit: u64) -> RecordOp<op::Patch> {
    RecordOp::patch(key(n), BUDGET)
        // The new value usually differs, which moves the record between two large
        // index terms.
        .attribute("one_of_5", CellValue::from_u64((n + commit) % 5))
        .attribute("last_commit", CellValue::from_u64(commit))
        .field("payload", payload(commit))
}

/// Returns 1 KiB of bytes that depend on the commit, so that every patch writes
/// new content.
fn payload(commit: u64) -> CellValue {
    CellValue::from_bytes(&commit.to_le_bytes().repeat(128))
}
