//! Enumeration over one record's live cells and historical names.

use std::iter::FusedIterator;

use golemdb_history::{CommitSet, HistoryError, ReadOperation};
use golemdb_storage::{Entry, ReadCursor, ReadTransaction};

use crate::{CellError, CellKey, CellValue, CellValueRef, Result, tables};

/// Lazy cell enumeration in name-byte order, bound to one storage snapshot.
/// Construction opens cursors without fetching rows. Live scans read only Cell;
/// historical scans merge Cell and CellHistory and skip cells absent at T.
/// Storage, codec or callback errors terminate the scan; subsequent calls do no
/// work. A missing record produces an empty iterator.
///
/// Historical work is proportional to candidate keys, their encoded commit sets
/// and selected values, not just emitted cells. A single `next` can skip many
/// absent candidates. Lookahead retains at most one live key and one history
/// row; decoding a history row also allocates its commit set. Live payloads are
/// not copied by lookahead. Use [`Self::next_with`] to control individual operations
/// and inspect borrowed payloads before copying them.
pub struct CellScan<'tx, R: ReadTransaction> {
    tx: &'tx R,
    live: R::Cursor<'tx>,
    history: Option<HistoricalScan<R::Cursor<'tx>>>,
    prefix: [u8; 8],
    done: bool,
}

impl<'tx, R: ReadTransaction> CellScan<'tx, R> {
    pub(crate) fn new(
        tx: &'tx R,
        record: u64,
        target: Option<u64>,
        first_name: &[u8],
    ) -> Result<Self> {
        let prefix = record.to_be_bytes();
        let start = [&prefix[..], first_name].concat();
        Ok(Self {
            tx,
            live: tx.cursor(tables::CELL, &start)?,
            history: target
                .map(|target| {
                    Ok::<_, CellError>(HistoricalScan {
                        target,
                        cursor: tx.cursor(tables::CELL_HISTORY, &start)?,
                        live_key: None,
                        history_row: None,
                        live_done: false,
                        history_done: false,
                    })
                })
                .transpose()?,
            prefix,
            done: false,
        })
    }

    /// Return the next present cell through a borrowed-value callback. `before`
    /// authorizes each cursor advance and selected-value lookup, and each
    /// commit-set package before its first copy/decode. Buffered history is
    /// charged once, even when selected on a later call. `inspect` runs before
    /// domain parsing/copying. Empty pre-images mean absence; empty live values
    /// remain visible to validation. Any error terminates the scan.
    pub fn next_with<C, T, E: From<CellError>>(
        &mut self,
        context: &mut C,
        mut before: impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
        mut inspect: impl FnMut(&mut C, CellKey, &[u8]) -> std::result::Result<T, E>,
    ) -> std::result::Result<Option<T>, E> {
        if self.done {
            return Ok(None);
        }
        let result = self.advance(context, &mut before, &mut inspect);
        if !matches!(result, Ok(Some(_))) {
            self.done = true;
        }
        result
    }

    fn advance<C, T, E: From<CellError>>(
        &mut self,
        context: &mut C,
        before: &mut impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
        inspect: &mut impl FnMut(&mut C, CellKey, &[u8]) -> std::result::Result<T, E>,
    ) -> std::result::Result<Option<T>, E> {
        loop {
            let Some(history) = &mut self.history else {
                before(context, ReadOperation::Advance(tables::CELL))?;
                return self
                    .live
                    .with_next(|row| {
                        row.filter(|(key, _)| key.starts_with(&self.prefix))
                            .map(|(key, bytes)| {
                                let key = CellKey::decode(key).map_err(CellError::from)?;
                                inspect(context, key, bytes)
                            })
                            .transpose()
                    })
                    .map_err(CellError::from)?;
            };
            let Some(Candidate { encoded, commits }) =
                history.next_candidate(&mut self.live, &self.prefix, context, before)?
            else {
                return Ok(None);
            };
            let key = CellKey::decode(&encoded).map_err(CellError::from)?;
            let successor = commits
                .map(|bytes| {
                    CommitSet::decode(&bytes)
                        .map(|commits| commits.first_after(history.target))
                        .map_err(|source| {
                            CellError::History(HistoryError::InvalidCommits {
                                table: tables::CELL_HISTORY,
                                key: encoded.clone(),
                                source,
                            })
                        })
                })
                .transpose()?
                .flatten();
            let value = match successor {
                Some(commit) => {
                    before(context, ReadOperation::Lookup(tables::CELL_CHANGE_SET))?;
                    tables::HISTORY
                        .with_preimage(self.tx, &encoded, commit, |bytes| {
                            if bytes.is_empty() {
                                Ok(None)
                            } else {
                                inspect(context, key, bytes).map(Some)
                            }
                        })
                        .map_err(CellError::from)??
                }
                None => {
                    before(context, ReadOperation::Lookup(tables::CELL))?;
                    self.tx
                        .with_value(tables::CELL, &encoded, |bytes| {
                            bytes.map(|bytes| inspect(context, key, bytes)).transpose()
                        })
                        .map_err(CellError::from)??
                }
            };
            if value.is_some() {
                return Ok(value);
            }
        }
    }
}

impl<R: ReadTransaction> Iterator for CellScan<'_, R> {
    type Item = Result<(CellKey, CellValue)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_with(
            &mut (),
            |_, _| Ok::<_, CellError>(()),
            |_, key, bytes| Ok((key, CellValueRef::parse(bytes)?.into())),
        )
        .transpose()
    }
}

impl<R: ReadTransaction> FusedIterator for CellScan<'_, R> {}

struct HistoricalScan<C> {
    target: u64,
    cursor: C,
    live_key: Option<Vec<u8>>,
    history_row: Option<Entry>,
    live_done: bool,
    history_done: bool,
}

struct Candidate {
    encoded: Vec<u8>,
    commits: Option<Vec<u8>>,
}

impl<Cursor: ReadCursor> HistoricalScan<Cursor> {
    fn next_candidate<C, E: From<CellError>>(
        &mut self,
        live: &mut Cursor,
        prefix: &[u8],
        context: &mut C,
        before: &mut impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
    ) -> std::result::Result<Option<Candidate>, E> {
        if self.live_key.is_none() && !self.live_done {
            before(context, ReadOperation::Advance(tables::CELL))?;
            self.live_key = live
                .with_next(|row| {
                    row.filter(|(key, _)| key.starts_with(prefix))
                        .map(|(key, _)| key.to_vec())
                })
                .map_err(CellError::from)?;
            self.live_done = self.live_key.is_none();
        }
        if self.history_row.is_none() && !self.history_done {
            before(context, ReadOperation::Advance(tables::CELL_HISTORY))?;
            self.history_row = self
                .cursor
                .with_next(|row| {
                    row.filter(|(key, _)| key.starts_with(prefix))
                        .map(|(key, bytes)| {
                            before(
                                context,
                                ReadOperation::CommitSet {
                                    encoded_bytes: bytes.len() as u64,
                                },
                            )?;
                            Ok::<_, E>((key.to_vec(), bytes.to_vec()))
                        })
                        .transpose()
                })
                .map_err(CellError::from)??;
            self.history_done = self.history_row.is_none();
        }
        if self
            .history_row
            .as_ref()
            .is_some_and(|(key, _)| self.live_key.as_ref().is_none_or(|live| key <= live))
        {
            let (key, commits) = self.history_row.take().expect("pending history row");
            if self.live_key.as_ref() == Some(&key) {
                self.live_key = None;
            }
            Ok(Some(Candidate {
                encoded: key,
                commits: Some(commits),
            }))
        } else {
            Ok(self.live_key.take().map(|key| Candidate {
                encoded: key,
                commits: None,
            }))
        }
    }
}
