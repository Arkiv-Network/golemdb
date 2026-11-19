//! Ordered container discovery, temporal resolution, and shared term assembly.

use std::{iter::FusedIterator, ops::Bound};

use golemdb_history::{CommitSet, HistoryError, ReadOperation};
use golemdb_storage::{Entry, ReadCursor, ReadTransaction};

use crate::{
    Bitmap, BitmapContainer, IndexError, IndexKey, IndexTerm, Result, TermRange, key, tables,
    term_range::past,
};

/// Lazy containers in `(term, hi48)` order, bound to one storage snapshot.
/// Historical scans merge live and historical keys, then resolve each container
/// independently. Construction opens cursors without fetching rows.
///
/// One `next` may skip many candidates absent at the target. Lookahead copies
/// at most one live key and one encoded history row, never live payloads;
/// decoding history also allocates its commit set. Use [`Self::next_with`] to
/// control individual operations and inspect selected bytes before decoding/copying.
/// Any storage, codec or callback error ends the scan permanently.
pub struct ContainerScan<'tx, R: ReadTransaction> {
    tx: &'tx R,
    live: R::Cursor<'tx>,
    history: Option<HistoricalScan<R::Cursor<'tx>>>,
    upper: Bound<Vec<u8>>,
    done: bool,
}

impl<'tx, R: ReadTransaction> ContainerScan<'tx, R> {
    pub(crate) fn for_term(tx: &'tx R, target: Option<u64>, term: &IndexTerm) -> Result<Self> {
        Self::new(tx, target, term.as_bytes(), past(term.as_bytes().to_vec()))
    }

    pub(crate) fn for_range(tx: &'tx R, target: Option<u64>, range: TermRange) -> Result<Self> {
        let (lower, upper) = key::bounds(range.bounds());
        let start = match lower {
            Bound::Included(start) => start,
            Bound::Unbounded => Vec::new(),
            Bound::Excluded(_) => unreachable!("key::bounds normalizes excluded lower terms"),
        };
        Self::new(tx, target, &start, upper)
    }

    fn new(tx: &'tx R, target: Option<u64>, start: &[u8], upper: Bound<Vec<u8>>) -> Result<Self> {
        Ok(Self {
            tx,
            live: tx.cursor(tables::INDEX, start)?,
            history: target
                .map(|target| {
                    Ok::<_, IndexError>(HistoricalScan {
                        target,
                        cursor: tx.cursor(tables::INDEX_HISTORY, start)?,
                        live_key: None,
                        history_row: None,
                        live_done: false,
                        history_done: false,
                    })
                })
                .transpose()?,
            upper,
            done: false,
        })
    }

    /// Authorize each physical operation and bitmap package before it runs.
    /// A buffered history row is processed/charged once and reused on selection.
    pub fn next_with<C, T, E: From<IndexError>>(
        &mut self,
        context: &mut C,
        mut before: impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
        mut inspect: impl FnMut(&mut C, IndexKey, &[u8]) -> std::result::Result<T, E>,
    ) -> std::result::Result<Option<T>, E> {
        loop {
            match self.next_candidate_with(context, &mut before, |context, key, raw| {
                raw.map(|raw| inspect(context, key, raw)).transpose()
            })? {
                Some(Some(value)) => return Ok(Some(value)),
                Some(None) => continue,
                None => return Ok(None),
            }
        }
    }

    // Keep absent candidates visible to term assembly so it can observe the
    // next term boundary without scanning arbitrarily many absent later terms.
    fn next_candidate_with<Context, Output, E: From<IndexError>>(
        &mut self,
        context: &mut Context,
        mut before: impl FnMut(&mut Context, ReadOperation) -> std::result::Result<(), E>,
        mut inspect: impl FnMut(&mut Context, IndexKey, Option<&[u8]>) -> std::result::Result<Output, E>,
    ) -> std::result::Result<Option<Output>, E> {
        if self.done {
            return Ok(None);
        }
        let result = self.read_candidate(context, &mut before, &mut inspect);
        if !matches!(result, Ok(Some(_))) {
            self.done = true;
        }
        result
    }

    fn read_candidate<Context, Output, E: From<IndexError>>(
        &mut self,
        context: &mut Context,
        before: &mut impl FnMut(&mut Context, ReadOperation) -> std::result::Result<(), E>,
        inspect: &mut impl FnMut(
            &mut Context,
            IndexKey,
            Option<&[u8]>,
        ) -> std::result::Result<Output, E>,
    ) -> std::result::Result<Option<Output>, E> {
        let Some(history) = &mut self.history else {
            before(context, ReadOperation::Advance(tables::INDEX))?;
            return self
                .live
                .with_next(|row| {
                    row.filter(|(key, _)| within(key, &self.upper))
                        .map(|(key, bytes)| inspect(context, IndexKey::decode(key)?, Some(bytes)))
                        .transpose()
                })
                .map_err(IndexError::from)?;
        };
        let Some(Candidate { encoded, commits }) =
            history.next_candidate(&mut self.live, &self.upper, context, before)?
        else {
            return Ok(None);
        };
        let key = IndexKey::decode(&encoded)?;
        let successor = commits
            .map(|bytes| {
                CommitSet::decode(&bytes)
                    .map(|commits| commits.first_after(history.target))
                    .map_err(|source| {
                        IndexError::History(HistoryError::InvalidCommits {
                            table: tables::INDEX_HISTORY,
                            key: encoded.clone(),
                            source,
                        })
                    })
            })
            .transpose()?
            .flatten();
        let value = match successor {
            Some(commit) => {
                before(context, ReadOperation::Lookup(tables::INDEX_CHANGE_SET))?;
                tables::HISTORY
                    .with_preimage(self.tx, &encoded, commit, |bytes| {
                        inspect(context, key, bytes)
                    })
                    .map_err(IndexError::from)??
            }
            None => {
                before(context, ReadOperation::Lookup(tables::INDEX))?;
                self.tx
                    .with_value(tables::INDEX, &encoded, |bytes| {
                        inspect(context, key, bytes)
                    })
                    .map_err(IndexError::from)??
            }
        };
        Ok(Some(value))
    }
}

impl<R: ReadTransaction> Iterator for ContainerScan<'_, R> {
    type Item = Result<(IndexKey, BitmapContainer)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_with(
            &mut (),
            |_, _| Ok::<_, IndexError>(()),
            |_, key, bytes| {
                let container = BitmapContainer::decode(key.container_id, bytes)?;
                Ok((key, container))
            },
        )
        .transpose()
    }
}

impl<R: ReadTransaction> FusedIterator for ContainerScan<'_, R> {}

fn within(key: &[u8], upper: &Bound<Vec<u8>>) -> bool {
    match upper {
        Bound::Unbounded => true,
        Bound::Included(end) => key <= end.as_slice(),
        Bound::Excluded(end) => key < end.as_slice(),
    }
}

struct HistoricalScan<Cursor> {
    target: u64,
    cursor: Cursor,
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
    fn next_candidate<C, E: From<IndexError>>(
        &mut self,
        live: &mut Cursor,
        upper: &Bound<Vec<u8>>,
        context: &mut C,
        before: &mut impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
    ) -> std::result::Result<Option<Candidate>, E> {
        if self.live_key.is_none() && !self.live_done {
            before(context, ReadOperation::Advance(tables::INDEX))?;
            self.live_key = live
                .with_next(|row| {
                    row.filter(|(key, _)| within(key, upper))
                        .map(|(key, _)| key.to_vec())
                })
                .map_err(IndexError::from)?;
            self.live_done = self.live_key.is_none();
        }
        if self.history_row.is_none() && !self.history_done {
            before(context, ReadOperation::Advance(tables::INDEX_HISTORY))?;
            self.history_row = self
                .cursor
                .with_next(|row| {
                    row.filter(|(key, _)| within(key, upper))
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
                .map_err(IndexError::from)??;
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

/// Shared live/historical term assembly. Materializes one term's bitmap and
/// retains at most one resolved candidate from the following term as lookahead.
/// A single term may require many candidate reads; use [`ContainerScan`] for
/// finer work control. Errors terminate iteration without emitting a partial term.
pub struct TermScan<'tx, R: ReadTransaction> {
    rows: ContainerScan<'tx, R>,
    pending: Option<(IndexKey, Option<BitmapContainer>)>,
    done: bool,
}

impl<'tx, R: ReadTransaction> TermScan<'tx, R> {
    pub(crate) fn new(rows: ContainerScan<'tx, R>) -> Self {
        Self {
            rows,
            pending: None,
            done: false,
        }
    }

    /// Forward storage/commit-set checkpoints, and authorize each selected
    /// container before decoding it. Query pricing policy belongs to the caller.
    /// One resolved candidate of the following term can be retained as lookahead.
    pub fn next_with<C, E: From<IndexError>>(
        &mut self,
        context: &mut C,
        mut before: impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
        mut before_container: impl FnMut(&mut C, &IndexKey, &[u8]) -> std::result::Result<(), E>,
    ) -> std::result::Result<Option<(IndexTerm, Bitmap)>, E> {
        if self.done {
            return Ok(None);
        }
        let result = self.step(context, &mut before, &mut before_container);
        if !matches!(result, Ok(Some(_))) {
            self.done = true;
        }
        result
    }

    fn step<C, E: From<IndexError>>(
        &mut self,
        context: &mut C,
        before: &mut impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
        before_container: &mut impl FnMut(&mut C, &IndexKey, &[u8]) -> std::result::Result<(), E>,
    ) -> std::result::Result<Option<(IndexTerm, Bitmap)>, E> {
        loop {
            let Some((key, container)) = self.candidate(context, before, before_container)? else {
                return Ok(None);
            };
            let term = key.term;
            let mut containers: Vec<_> = container.into_iter().collect();
            // Even an absent candidate marks a term boundary. A failure before
            // that boundary could hide another container, so discard the term.
            while let Some((key, container)) = self.candidate(context, before, before_container)? {
                if key.term != term {
                    self.pending = Some((key, container));
                    break;
                }
                containers.extend(container);
            }
            if !containers.is_empty() {
                return Ok(Some((
                    term,
                    Bitmap::from_containers(containers).map_err(IndexError::from)?,
                )));
            }
        }
    }

    fn candidate<C, E: From<IndexError>>(
        &mut self,
        context: &mut C,
        before: &mut impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
        before_container: &mut impl FnMut(&mut C, &IndexKey, &[u8]) -> std::result::Result<(), E>,
    ) -> std::result::Result<Option<(IndexKey, Option<BitmapContainer>)>, E> {
        if self.pending.is_some() {
            return Ok(self.pending.take());
        }
        self.rows
            .next_candidate_with(context, before, |context, key, raw| {
                let container = raw
                    .map(|raw| {
                        before_container(context, &key, raw)?;
                        BitmapContainer::decode(key.container_id, raw)
                            .map_err(IndexError::from)
                            .map_err(E::from)
                    })
                    .transpose()?;
                Ok((key, container))
            })
    }
}

impl<R: ReadTransaction> Iterator for TermScan<'_, R> {
    type Item = Result<(IndexTerm, Bitmap)>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_with(&mut (), |_, _| Ok(()), |_, _, _| Ok(()))
            .transpose()
    }
}
impl<R: ReadTransaction> FusedIterator for TermScan<'_, R> {}
