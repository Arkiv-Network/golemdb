use std::ops::Bound;

use golemdb_branch::{
    BranchError, CommitId, ROOTS, Roots, head_roots, read_head_state, read_roots,
};
use golemdb_cells::{
    CELL_BRANCH_DOMAIN, CellKey, CellValueRef, reserved, system, tables::CELL, tables::CELL_TRIE,
};
use golemdb_index::{
    BITMAP_BRANCH_DOMAIN, BITMAP_LEAF_DOMAIN, BITMAP_TRIE_PATH_BYTES, INDEX_BRANCH_DOMAIN,
    tables::{BITMAP_CONTAINER, BITMAP_TRIE, INDEX, INDEX_TRIE},
};
use golemdb_merkle::{
    BRANCH_SLOTS, BranchNodeCompact, HASH_BYTES, HashProvider, LeafStorage, MerkleError,
    PayloadTrie, RootRef, Trie, TrieTable,
};
use golemdb_pruning::Pruner;
use golemdb_storage::{ReadTransaction, Store, Table, WriteTransaction, scan};

use crate::{PruneError, PruneOptions, Report, Result, write::Writer};

/// Mark in `marks` every row the retained roots reach, and list there in
/// `Stale` those the head does not. Returns the report and the cutoff.
pub(crate) fn mark<H: HashProvider>(
    store: &impl Store,
    marks: &impl Store,
    options: &PruneOptions,
    hasher: &H,
) -> Result<(Report, u64)> {
    let window = options.config.window();
    let read = store.begin_read()?;
    let head = read_head_state(&read)?;
    head_roots(&read, &head, hasher)?;
    let head = head.commit_id.get();
    let min = min_retention(&read)?;
    if window < min {
        return Err(PruneError::BelowMinRetention { window, min });
    }
    let pruner = &Pruner::new(options.config);
    let mut out = Writer::new(marks, options.batch_rows)?;
    if !out.tx().is_empty()? {
        return Err(PruneError::MarksNotEmpty);
    }

    let cutoff = pruner.cutoff(head);
    let lower = Bound::Included(cutoff.to_be_bytes().to_vec());
    let upper = Bound::Included(head.to_be_bytes().to_vec());
    let retained =
        scan(&read, ROOTS, lower, upper)?.collect::<golemdb_storage::Result<Vec<_>>>()?;
    let cells = Marker::new(&read, CELL_TRIE, CELL_BRANCH_DOMAIN, hasher);
    let index = Marker::new(&read, INDEX_TRIE, INDEX_BRANCH_DOMAIN, hasher);
    let mut report = Report {
        head,
        earliest: head,
        ..Report::default()
    };
    // Newest first: the first root to reach a row is the last that did.
    let mut newer = head;
    for (key, _) in retained.iter().rev() {
        let commit = commit_of(key)?;
        let Roots {
            state,
            index: index_root,
        } = read_roots(&read, CommitId::new(commit))?.ok_or(BranchError::MissingRoots(commit))?;
        let superseded_at = (commit != head).then_some(newer);
        cells.mark(&mut out, pruner, state, superseded_at, &mut report)?;
        index.mark(&mut out, pruner, index_root, superseded_at, &mut report)?;
        report.earliest = commit;
        newer = commit;
    }
    for row in scan(&read, INDEX, Bound::Unbounded, Bound::Unbounded)? {
        let (term, root) = row?;
        let root = RootRef::decode(&root).map_err(|_| PruneError::Corrupt("Index row"))?;
        mark_bitmap(&read, &mut out, hasher, &term, root, &mut report)?;
    }
    out.finish()?;
    Ok((report, cutoff))
}

fn min_retention(tx: &impl ReadTransaction) -> Result<u64> {
    let key = CellKey::new(system::PARAMS.id, reserved::MIN_RETENTION);
    let bytes = (tx.get(CELL, &key.encode())?).ok_or(PruneError::Corrupt("no #minRetention"))?;
    let value = CellValueRef::parse(&bytes)
        .ok()
        .and_then(|value| value.as_u64());
    value.ok_or(PruneError::Corrupt("#minRetention is not a u64"))
}

fn commit_of(key: &[u8]) -> Result<u64> {
    let bytes = key
        .try_into()
        .map_err(|_| BranchError::InvalidRootsKey(key.to_vec()))?;
    Ok(u64::from_be_bytes(bytes))
}

struct Marker<'a, T, H: HashProvider, const N: usize> {
    store: &'a T,
    table: Table,
    domain: u8,
    trie: Trie<'a, H, N>,
    hasher: &'a H,
}

impl<'a, T: ReadTransaction, H: HashProvider, const N: usize> Marker<'a, T, H, N> {
    fn new(store: &'a T, table: TrieTable, domain: u8, hasher: &'a H) -> Self {
        Self {
            store,
            table: table.table(),
            domain,
            trie: Trie::new(table, domain, hasher),
            hasher,
        }
    }

    /// Mark the unmarked rows `root` reaches, listing them at `superseded_at`.
    /// A marked row's subtree is already marked.
    fn mark<S: Store>(
        &self,
        out: &mut Writer<'_, S>,
        pruner: &Pruner,
        root: RootRef<N>,
        superseded_at: Option<u64>,
        report: &mut Report,
    ) -> Result<()> {
        let mut stack = vec![(root, Vec::new())];
        while let Some((root, route)) = stack.pop() {
            let RootRef::Branch(branch) = root else {
                continue;
            };
            let key = self.trie.node_key(branch.commit, &route);
            if out.tx().get(self.table, &key)?.is_some() {
                continue;
            }
            let node = stored_row(self.store, self.table, &key, branch.commit)?;
            let node = BranchNodeCompact::<N>::decode(&node, LeafStorage::InNode)?;
            if node.hash(self.domain, self.hasher) != branch.hash {
                return Err(MerkleError::HashMismatch.into());
            }
            out.put(self.table, &key, &[])?;
            report.kept += 1;
            if let Some(commit) = superseded_at {
                pruner.register_tries(out.tx(), commit, [(self.table, &key[..])])?;
                out.wrote()?;
                report.listed += 1;
            }
            push_children(&mut stack, &route, &node);
        }
        Ok(())
    }
}

pub(crate) fn bitmap_trie<'h, H: HashProvider>(
    hasher: &'h H,
    term: &[u8],
) -> PayloadTrie<'h, H, BITMAP_TRIE_PATH_BYTES, HASH_BYTES> {
    PayloadTrie::namespaced(
        BITMAP_TRIE,
        BITMAP_CONTAINER,
        hasher.hash(term),
        BITMAP_BRANCH_DOMAIN,
        hasher,
    )
}

fn mark_bitmap<S: Store, H: HashProvider>(
    store: &impl ReadTransaction,
    out: &mut Writer<'_, S>,
    hasher: &H,
    term: &[u8],
    root: RootRef<BITMAP_TRIE_PATH_BYTES>,
    report: &mut Report,
) -> Result<()> {
    let trie = bitmap_trie(hasher, term);
    let mut stack = vec![(root, Vec::new())];
    while let Some((root, route)) = stack.pop() {
        match root {
            RootRef::Empty => continue,
            RootRef::Leaf(leaf) => {
                let table = BITMAP_CONTAINER.table();
                let key = trie.leaf_key(leaf.commit, &leaf.path);
                let container = stored_row(store, table, &key, leaf.commit)?;
                if hasher.hash_parts(&[&[BITMAP_LEAF_DOMAIN], &container]) != leaf.hash {
                    return Err(MerkleError::HashMismatch.into());
                }
                out.put(table, &key, &[])?;
            }
            RootRef::Branch(branch) => {
                let table = BITMAP_TRIE.table();
                let key = trie.node_key(branch.commit, &route);
                let node = stored_row(store, table, &key, branch.commit)?;
                let node = BranchNodeCompact::decode(&node, LeafStorage::Rows)?;
                if node.hash(BITMAP_BRANCH_DOMAIN, hasher) != branch.hash {
                    return Err(MerkleError::HashMismatch.into());
                }
                out.put(table, &key, &[])?;
                push_children(&mut stack, &route, &node);
            }
        }
        report.kept += 1;
    }
    Ok(())
}

fn stored_row(
    store: &impl ReadTransaction,
    table: Table,
    key: &[u8],
    commit: u64,
) -> Result<Vec<u8>> {
    store.get(table, key)?.ok_or(PruneError::MissingRow {
        table: table.0,
        commit,
    })
}

fn push_children<const N: usize>(
    stack: &mut Vec<(RootRef<N>, Vec<u8>)>,
    route: &[u8],
    node: &BranchNodeCompact<N>,
) {
    let base = [route, &node.unpack_prefix()].concat();
    for slot in 0..BRANCH_SLOTS as u8 {
        if let Some(child) = node.child(slot) {
            stack.push((child, [&base[..], &[slot]].concat()));
        }
    }
}
