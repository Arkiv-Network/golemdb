//! Persistent reference counting for physical trie and bitmap nodes.
//!
//! Retained commits own their cell/index trie roots; their index terms own
//! bitmap roots. `NodeRefs` counts these root references and edges from live
//! physical parents. Each parent's edges count once, regardless of how many
//! owners that parent has. A zero count releases children and deletes the node.
//!
//! [`transfer`] acquires new roots before releasing old ones, so shared nodes
//! never pass through zero. The retention policy in [`update`] chooses what a
//! commit releases. Only head-only retention is implemented: a commit releases
//! its origin's roots. A retention window would instead release the roots of
//! the commit leaving it, read from `#roots` and the index change set.
//!
//! A node reaches zero only when the commit's trie updates replaced it, so
//! they have already loaded and authenticated it; GC only decodes its edges.
//! Seal stages counts and deletions in its buffer; publication replays them
//! atomically with head after validating the origin.
use std::collections::{BTreeMap, BTreeSet};

use golemdb_cells::{CELL_TRIE_PATH_BYTES, CellsUpdate, tables as cells};
use golemdb_index::{BITMAP_TRIE_PATH_BYTES, INDEX_TRIE_PATH_BYTES, IndexUpdate, tables as index};
use golemdb_merkle::{BranchNodeCompact, Hash, RootRef};
use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

use crate::{BranchError, Result, buffer::Buffered, tables::NODE_REFS};

/// Commits before head whose physical tries are retained. Zero is head-only.
pub(crate) const RETENTION: u64 = 0;

/// Reject a database whose retention policy is not implemented.
pub(crate) fn check_retention(retention: u64) -> Result<()> {
    if retention != RETENTION {
        return Err(BranchError::UnsupportedRetention(retention));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
enum Kind {
    Cell = 0,
    Index = 1,
    Bitmap = 2,
    Container = 3,
}

impl Kind {
    fn table(self) -> Table {
        match self {
            Self::Cell => cells::CELL_TRIE,
            Self::Index => index::INDEX_TRIE,
            Self::Bitmap => index::BITMAP_TRIE,
            Self::Container => index::BITMAP_CONTAINER,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Node {
    kind: Kind,
    hash: Hash,
}

impl Node {
    fn key(self) -> [u8; 33] {
        let mut key = [0; 33];
        key[0] = self.kind as u8;
        key[1..].copy_from_slice(&self.hash);
        key
    }
}

fn root<const N: usize>(kind: Kind, root: RootRef<N>) -> Option<Node> {
    match root {
        RootRef::Branch(hash) => Some(Node { kind, hash }),
        _ => None, // Empty tries and cell/index leaves have no physical row.
    }
}

/// Physical roots referenced by one owner.
#[derive(Default)]
pub(crate) struct Roots(Vec<Node>);

impl Roots {
    pub(crate) fn tries(
        cells: RootRef<CELL_TRIE_PATH_BYTES>,
        index: RootRef<INDEX_TRIE_PATH_BYTES>,
    ) -> Self {
        Self(
            [root(Kind::Cell, cells), root(Kind::Index, index)]
                .into_iter()
                .flatten()
                .collect(),
        )
    }

    /// Add a term's bitmap root. A single-container bitmap's root hash is the
    /// container's own hash; domain separation keeps it apart from branches.
    pub(crate) fn bitmap(&mut self, tx: &impl ReadTransaction, hash: Hash) -> Result<()> {
        let kind = if tx.get(index::BITMAP_CONTAINER, &hash)?.is_some() {
            Kind::Container
        } else {
            Kind::Bitmap
        };
        self.0.push(Node { kind, hash });
        Ok(())
    }
}

/// Apply the retention policy to one commit from `origin` to the new state.
/// Head-only retention releases the origin's roots and changed terms' old
/// bitmap roots. Genesis is the commit from two empty tries.
pub(crate) fn update<R: ReadTransaction>(
    tx: &mut Buffered<'_, R>,
    retention: u64,
    origin_cells: RootRef<CELL_TRIE_PATH_BYTES>,
    origin_index: RootRef<INDEX_TRIE_PATH_BYTES>,
    cells: &CellsUpdate,
    index: &IndexUpdate,
) -> Result<()> {
    check_retention(retention)?;
    let mut acquired = Roots::tries(cells.root, index.root);
    let mut released = Roots::tries(origin_cells, origin_index);
    for term in &index.changed_terms {
        if let Some(hash) = term.after {
            acquired.bitmap(tx, hash)?;
        }
        if let Some(hash) = term.before {
            released.bitmap(tx, hash)?;
        }
    }
    transfer(tx, acquired, released)
}

/// Acquire, then release, then stage changed counts and deletions in the
/// buffer. Rows already written to the buffer are new nodes: the only live
/// nodes allowed without a stored count. Old roots stay readable until then.
pub(crate) fn transfer<R: ReadTransaction>(
    tx: &mut Buffered<'_, R>,
    acquired: Roots,
    released: Roots,
) -> Result<()> {
    let mut created = BTreeSet::new();
    for kind in [Kind::Cell, Kind::Index, Kind::Bitmap, Kind::Container] {
        let Some(rows) = tx.writes().get(&kind.table()) else {
            continue;
        };
        for (key, value) in rows {
            if value.is_some() {
                let hash = key.as_slice().try_into().map_err(|_| {
                    BranchError::GarbageCollection("immutable row key must be 32 bytes")
                })?;
                created.insert(Node { kind, hash });
            }
        }
    }
    let mut session = Session {
        tx,
        counts: BTreeMap::new(),
        created,
    };
    for node in acquired.0 {
        session.retain(node)?;
    }
    for node in released.0 {
        session.release(node)?;
    }
    session.flush()
}

struct Counter {
    before: u64,
    after: u64,
}

struct Session<'tx, T> {
    tx: &'tx mut T,
    counts: BTreeMap<Node, Counter>,
    created: BTreeSet<Node>,
}

impl<T: WriteTransaction> Session<'_, T> {
    fn counter(&mut self, node: Node) -> Result<&mut Counter> {
        if !self.counts.contains_key(&node) {
            let before = self.stored_count(node)?;
            self.counts.insert(
                node,
                Counter {
                    before,
                    after: before,
                },
            );
        }
        Ok(self.counts.get_mut(&node).unwrap())
    }

    fn stored_count(&self, node: Node) -> Result<u64> {
        match self.tx.get(NODE_REFS, &node.key())? {
            None if self.created.contains(&node) => Ok(0),
            None => Err(BranchError::GarbageCollection(
                "live node has no reference count",
            )),
            Some(bytes) => {
                let count = bytes.try_into().map(u64::from_be_bytes).map_err(|_| {
                    BranchError::GarbageCollection("reference count must be eight bytes")
                })?;
                if count == 0 {
                    return Err(BranchError::GarbageCollection(
                        "stored reference count is zero",
                    ));
                }
                Ok(count)
            }
        }
    }

    fn retain(&mut self, node: Node) -> Result<()> {
        let mut pending = vec![node];
        while let Some(node) = pending.pop() {
            let counter = self.counter(node)?;
            let activate = counter.after == 0;
            counter.after = counter
                .after
                .checked_add(1)
                .ok_or(BranchError::GarbageCollection("reference count overflow"))?;
            if activate {
                pending.extend(self.children(node)?);
            }
        }
        Ok(())
    }

    fn release(&mut self, node: Node) -> Result<()> {
        let mut pending = vec![node];
        while let Some(node) = pending.pop() {
            let counter = self.counter(node)?;
            counter.after = counter
                .after
                .checked_sub(1)
                .ok_or(BranchError::GarbageCollection("reference count underflow"))?;
            if counter.after == 0 {
                pending.extend(self.children(node)?);
            }
        }
        Ok(())
    }

    fn children(&self, node: Node) -> Result<Vec<Node>> {
        match node.kind {
            Kind::Cell => self.branches::<CELL_TRIE_PATH_BYTES>(node, None),
            Kind::Index => self.branches::<INDEX_TRIE_PATH_BYTES>(node, None),
            Kind::Bitmap => self.branches::<BITMAP_TRIE_PATH_BYTES>(node, Some(Kind::Container)),
            // Containers have no outgoing edges.
            Kind::Container => Ok(Vec::new()),
        }
    }

    /// Cell/index leaves have no physical row; bitmap leaves are containers.
    fn branches<const N: usize>(&self, node: Node, leaves: Option<Kind>) -> Result<Vec<Node>> {
        let bytes = self
            .tx
            .get(node.kind.table(), &node.hash)?
            .ok_or(BranchError::GarbageCollection("referenced node is missing"))?;
        let branch = BranchNodeCompact::<N>::decode(&bytes)?;
        let mut children = Vec::new();
        for slot in 0..16 {
            match branch.child(slot) {
                Some(RootRef::Branch(hash)) => children.push(Node {
                    kind: node.kind,
                    hash,
                }),
                Some(RootRef::Leaf(leaf)) => {
                    children.extend(leaves.map(|kind| Node {
                        kind,
                        hash: leaf.hash,
                    }));
                }
                _ => {}
            }
        }
        Ok(children)
    }

    fn flush(self) -> Result<()> {
        for (node, counter) in self.counts {
            if counter.before == counter.after {
                continue;
            }
            if counter.after == 0 {
                self.tx.delete(NODE_REFS, &node.key())?;
                self.tx.delete(node.kind.table(), &node.hash)?;
            } else {
                self.tx
                    .put(NODE_REFS, &node.key(), &counter.after.to_be_bytes())?;
            }
        }
        Ok(())
    }
}
