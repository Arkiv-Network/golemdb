use golemdb_cells::{CellKey, CellValue};

use crate::record_scan::{OriginPosition, ScanIndex};
use crate::{BranchError, Result, overlay::Entries};

struct UndoEntry {
    key: CellKey,
    // None restores fall-through. Some(None) restores a tombstone.
    previous: Option<Option<CellValue>>,
    previous_position: Option<OriginPosition>,
}

/// State to restore if an atomic write group fails. Writes can reactivate the
/// current frame, but cannot change checkpoint markers through CellWrite.
#[derive(Clone, Copy)]
pub(crate) struct WriteMark {
    undo_len: usize,
    rolled_back: bool,
}

pub(crate) struct Journal {
    undo: Vec<UndoEntry>,
    // The last marker is always retained as the current frame's start.
    frames: Vec<usize>,
    // Distinguishes a frame emptied by rollback from a fresh empty frame.
    // Only the former is skipped by a consecutive rollback.
    rolled_back: bool,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            undo: Vec::new(),
            frames: vec![0],
            rolled_back: false,
        }
    }
}

impl Journal {
    pub(crate) fn version(&self) -> u64 {
        self.undo.len() as u64
    }

    pub(crate) fn mark(&self) -> WriteMark {
        WriteMark {
            undo_len: self.undo.len(),
            rolled_back: self.rolled_back,
        }
    }

    /// Apply a write and record its inverse, reusing the current frame after
    /// rollback. An explicit no-op write also reactivates the frame, without
    /// adding an unnecessary undo entry.
    pub(crate) fn record(
        &mut self,
        entries: &mut Entries,
        scan_index: &mut ScanIndex,
        key: CellKey,
        value: Option<CellValue>,
        position: Option<OriginPosition>,
    ) {
        self.rolled_back = false;
        if entries.get(&key) == Some(&value) {
            return;
        }
        let previous_position = scan_index.position(&key);
        scan_index.update(&key, Some(&value), position);
        let previous = entries.insert(key.clone(), value);
        self.undo.push(UndoEntry {
            key,
            previous,
            previous_position,
        });
    }

    pub(crate) fn checkpoint(&mut self) {
        // A rolled-back frame has already been consumed: reuse its retained
        // marker for the newly opened empty frame instead of sealing it again.
        if !self.rolled_back {
            self.frames.push(self.undo.len());
        }
        self.rolled_back = false;
    }

    pub(crate) fn rollback(
        &mut self,
        entries: &mut Entries,
        scan_index: &mut ScanIndex,
    ) -> Result<()> {
        if self.rolled_back {
            if self.frames.len() == 1 {
                return Err(BranchError::NoFrameToRollback);
            }
            self.frames.pop();
        }
        let mark = *self.frames.last().unwrap();
        self.undo_to(entries, scan_index, mark);
        self.rolled_back = true;
        Ok(())
    }

    pub(crate) fn rewind(
        &mut self,
        entries: &mut Entries,
        scan_index: &mut ScanIndex,
        mark: WriteMark,
    ) {
        self.undo_to(entries, scan_index, mark.undo_len);
        self.rolled_back = mark.rolled_back;
    }

    fn undo_to(&mut self, entries: &mut Entries, scan_index: &mut ScanIndex, mark: usize) {
        while self.undo.len() > mark {
            let UndoEntry {
                key,
                previous,
                previous_position,
            } = self.undo.pop().unwrap();
            scan_index.update(&key, previous.as_ref(), previous_position);
            match previous {
                Some(value) => {
                    entries.insert(key, value);
                }
                None => {
                    entries.remove(&key);
                }
            }
        }
    }
}
