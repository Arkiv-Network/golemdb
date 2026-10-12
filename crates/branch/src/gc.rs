//! Persistent reference counting for physical trie and bitmap nodes.
//!
//! Retained commits own their cell/index trie roots; their index terms own
//! bitmap roots. `NodeRefs` counts these root references and edges from live
//! physical parents. Each parent's edges count once, regardless of how many
//! owners that parent has. A zero count releases children and deletes the node.
//!
//! [`transfer`] first nets every count change in memory: acquired roots and
//! the children of nodes this commit created gain a reference, released roots
//! lose one. Only nodes left with a negative balance are checked against their
//! stored count; a node reaching zero passes the loss to its children. Nodes
//! whose balance nets to zero, such as siblings that a new branch copies from
//! the one it replaces, are never read or written. The retention policy in
//! [`update`] chooses what a commit releases. Only head-only retention is implemented: a commit releases
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

/// Net the count changes of moving from `released` to `acquired`, then stage
/// changed counts and deletions in the buffer. Rows already written to the
/// buffer are nodes this commit created: they start from zero, are reachable
/// from the acquired roots, and are the only live nodes without a stored count.
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
        created,
        balance: BTreeMap::new(),
        stored: BTreeMap::new(),
        dead: BTreeSet::new(),
    };
    for node in acquired.0 {
        session.add(node, 1)?;
    }
    for node in session.created.clone() {
        for child in session.children(node)? {
            session.add(child, 1)?;
        }
    }
    let mut pending = Vec::new();
    for node in released.0 {
        session.add(node, -1)?;
        pending.push(node);
    }
    // Nodes reach zero only after every gain is known, so shared nodes never do.
    while let Some(node) = pending.pop() {
        if session.balance[&node] >= 0 || session.dead.contains(&node) {
            continue;
        }
        let count = session.stored_count(node)?;
        match count.checked_add_signed(session.balance[&node]) {
            None => return Err(BranchError::GarbageCollection("reference count underflow")),
            Some(0) => {
                session.dead.insert(node);
                for child in session.children(node)? {
                    session.add(child, -1)?;
                    pending.push(child);
                }
            }
            Some(_) => {}
        }
    }
    for node in &session.created {
        if session
            .balance
            .get(node)
            .is_none_or(|&balance| balance <= 0)
        {
            return Err(BranchError::GarbageCollection(
                "created node is unreachable from the acquired roots",
            ));
        }
    }
    session.flush()
}

struct Session<'tx, 'r, R> {
    tx: &'tx mut Buffered<'r, R>,
    created: BTreeSet<Node>,
    /// Net reference change per node; zero entries need no storage access.
    balance: BTreeMap<Node, i64>,
    stored: BTreeMap<Node, u64>,
    dead: BTreeSet<Node>,
}

impl<R: ReadTransaction> Session<'_, '_, R> {
    fn add(&mut self, node: Node, change: i64) -> Result<()> {
        let balance = self.balance.entry(node).or_default();
        *balance = balance
            .checked_add(change)
            .ok_or(BranchError::GarbageCollection("reference count overflow"))?;
        Ok(())
    }

    fn stored_count(&mut self, node: Node) -> Result<u64> {
        if self.created.contains(&node) {
            return Ok(0);
        }
        if let Some(&count) = self.stored.get(&node) {
            return Ok(count);
        }
        let bytes = self
            .tx
            .get(NODE_REFS, &node.key())?
            .ok_or(BranchError::GarbageCollection(
                "live node has no reference count",
            ))?;
        let count = bytes
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| BranchError::GarbageCollection("reference count must be eight bytes"))?;
        if count == 0 {
            return Err(BranchError::GarbageCollection(
                "stored reference count is zero",
            ));
        }
        self.stored.insert(node, count);
        Ok(count)
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

    fn flush(mut self) -> Result<()> {
        let changed: Vec<_> = (self.balance.iter())
            .filter(|&(_, &balance)| balance != 0)
            .map(|(&node, &balance)| (node, balance))
            .collect();
        for (node, balance) in changed {
            if self.dead.contains(&node) {
                // Both rows were read to reach zero; skip the existence checks.
                self.tx.delete_existing(NODE_REFS, &node.key());
                self.tx.delete_existing(node.kind.table(), &node.hash);
                continue;
            }
            let count = self
                .stored_count(node)?
                .checked_add_signed(balance)
                .filter(|&count| count > 0)
                .ok_or(BranchError::GarbageCollection("reference count underflow"))?;
            self.tx.put(NODE_REFS, &node.key(), &count.to_be_bytes())?;
        }
        Ok(())
    }
}
