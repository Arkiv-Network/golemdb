//! Ordered container discovery, temporal resolution, and shared term assembly.

use std::{
    iter::{FusedIterator, Peekable},
    ops::Bound,
};

use golemdb_history::{CommitSet, HistoryError};
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
/// control candidate work and inspect selected bytes before decoding/copying.
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

    /// Inspect the next present container's borrowed bytes. `before_next` runs
    /// before every candidate attempt, including absent candidates and the final
    /// exhaustion probe; `inspect` runs before payload decoding or copying.
    /// Empty pre-images mean absence; empty live values remain malformed payloads.
    /// These are work hooks, not a pricing policy: physical attempts can depend
    /// on locally retained history and are not automatically chargeable units.
    pub fn next_with<Context, Output, E: From<IndexError>>(
        &mut self,
        context: &mut Context,
        mut before_next: impl FnMut(&mut Context) -> std::result::Result<(), E>,
        mut inspect: impl FnMut(&mut Context, IndexKey, &[u8]) -> std::result::Result<Output, E>,
    ) -> std::result::Result<Option<Output>, E> {
        loop {
            match self.next_candidate_with(context, &mut before_next, |context, key, bytes| {
                bytes.map(|bytes| inspect(context, key, bytes)).transpose()
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
        mut before_next: impl FnMut(&mut Context) -> std::result::Result<(), E>,
        mut inspect: impl FnMut(&mut Context, IndexKey, Option<&[u8]>) -> std::result::Result<Output, E>,
    ) -> std::result::Result<Option<Output>, E> {
        if self.done {
            return Ok(None);
        }
        let result = self.read_candidate(context, &mut before_next, &mut inspect);
        if !matches!(result, Ok(Some(_))) {
            self.done = true;
        }
        result
    }

    fn read_candidate<Context, Output, E: From<IndexError>>(
        &mut self,
        context: &mut Context,
        before_next: &mut impl FnMut(&mut Context) -> std::result::Result<(), E>,
        inspect: &mut impl FnMut(
            &mut Context,
            IndexKey,
            Option<&[u8]>,
        ) -> std::result::Result<Output, E>,
    ) -> std::result::Result<Option<Output>, E> {
        before_next(context)?;
        let Some(history) = &mut self.history else {
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
            history.next_candidate(&mut self.live, &self.upper)?
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
            Some(commit) => tables::HISTORY
                .with_preimage(self.tx, &encoded, commit, |bytes| {
                    inspect(context, key, (!bytes.is_empty()).then_some(bytes))
                })
                .map_err(IndexError::from)??,
            None => self
                .tx
                .with_value(tables::INDEX, &encoded, |bytes| {
                    inspect(context, key, bytes)
                })
                .map_err(IndexError::from)??,
        };
        Ok(Some(value))
    }
}

impl<R: ReadTransaction> Iterator for ContainerScan<'_, R> {
    type Item = Result<(IndexKey, BitmapContainer)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_with(
            &mut (),
            |_| Ok::<_, IndexError>(()),
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
    fn next_candidate(
        &mut self,
        live: &mut Cursor,
        upper: &Bound<Vec<u8>>,
    ) -> Result<Option<Candidate>> {
        if self.live_key.is_none() && !self.live_done {
            self.live_key = live.with_next(|row| {
                row.filter(|(key, _)| within(key, upper))
                    .map(|(key, _)| key.to_vec())
            })?;
            self.live_done = self.live_key.is_none();
        }
        if self.history_row.is_none() && !self.history_done {
            self.history_row = self.cursor.with_next(|row| {
                row.filter(|(key, _)| within(key, upper))
                    .map(|(key, bytes)| (key.to_vec(), bytes.to_vec()))
            })?;
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
    rows: Peekable<ResolvedCandidates<'tx, R>>,
    done: bool,
}

impl<'tx, R: ReadTransaction> TermScan<'tx, R> {
    pub(crate) fn new(rows: ContainerScan<'tx, R>) -> Self {
        Self {
            rows: ResolvedCandidates(rows).peekable(),
            done: false,
        }
    }

    fn step(&mut self) -> Result<Option<(IndexTerm, Bitmap)>> {
        loop {
            let Some(row) = self.rows.next() else {
                return Ok(None);
            };
            let (key, container) = row?;
            let term = key.term;
            let mut containers: Vec<_> = container.into_iter().collect();
            // An error could hide a remaining container of this term, so fail the
            // entire term rather than returning a partially assembled posting list.
            while let Some(row) = self
                .rows
                .next_if(|row| row.as_ref().map_or(true, |(key, _)| key.term == term))
            {
                let (_, container) = row?;
                containers.extend(container);
            }
            if !containers.is_empty() {
                return Ok(Some((term, Bitmap::from_containers(containers)?)));
            }
        }
    }
}

impl<R: ReadTransaction> Iterator for TermScan<'_, R> {
    type Item = Result<(IndexTerm, Bitmap)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = self.step().transpose();
        self.done = !matches!(item, Some(Ok(_)));
        item
    }
}

impl<R: ReadTransaction> FusedIterator for TermScan<'_, R> {}

// Unlike ContainerScan's public iterator, this keeps names of absent containers
// until their term has been assembled. Both use the same resolution mechanism.
struct ResolvedCandidates<'tx, R: ReadTransaction>(ContainerScan<'tx, R>);

impl<R: ReadTransaction> Iterator for ResolvedCandidates<'_, R> {
    type Item = Result<(IndexKey, Option<BitmapContainer>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0
            .next_candidate_with(
                &mut (),
                |_| Ok::<_, IndexError>(()),
                |_, key, bytes| {
                    let container = bytes
                        .map(|bytes| BitmapContainer::decode(key.container_id, bytes))
                        .transpose()?;
                    Ok((key, container))
                },
            )
            .transpose()
    }
}
