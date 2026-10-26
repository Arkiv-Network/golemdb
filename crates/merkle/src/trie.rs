use crate::{
    BranchNodeCompact, HASH_BYTES, Hash, HashProvider, MerkleError, Result, TrieTable,
    node::{BRANCH_SLOTS, check_path_width},
    path::{self, NIBBLES_PER_BYTE},
};
use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

/// The owner supplies a hash binding the complete path and its leaf payload.
/// `N` is the full routing path width in bytes, not the trie depth.
/// `commit` is kept for the root encoding; leaves have no rows, so it is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeafRef<const N: usize> {
    pub path: [u8; N],
    pub hash: Hash,
    pub commit: u64,
}

impl<const N: usize> LeafRef<N> {
    /// A leaf without a row of its own (commit 0: no row to locate).
    pub fn new(path: [u8; N], hash: Hash) -> Self {
        Self {
            path,
            hash,
            commit: 0,
        }
    }
}

/// A stored branch: its hash, and the commit that stored its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BranchRef {
    pub hash: Hash,
    pub commit: u64,
}

/// Bytes of a big-endian commit number: the start of every row key, and the
/// field after the hash in an encoded root.
pub const COMMIT_BYTES: usize = size_of::<u64>();

/// An encoded branch root, `hash ‖ commit`; a leaf root appends its path.
pub const BRANCH_ROOT_BYTES: usize = HASH_BYTES + COMMIT_BYTES;

/// Row key: the writing commit first, so each commit's rows are contiguous.
fn version_key(commit: u64, position: &[u8]) -> Vec<u8> {
    [&commit.to_be_bytes()[..], position].concat()
}

/// `N` is the full routing path width in bytes, not the trie depth.
/// Keep this metadata with retained roots. A bare singleton hash cannot recover
/// its path, and a missing branch must never be interpreted as a singleton.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RootRef<const N: usize> {
    #[default]
    Empty,
    Leaf(LeafRef<N>),
    Branch(BranchRef),
}

impl<const N: usize> RootRef<N> {
    /// Bytes of the longest encoding, a leaf root's.
    pub const MAX_ENCODED_BYTES: usize = BRANCH_ROOT_BYTES + N;

    /// Empty root = H(empty bytes); singleton root = its leaf hash.
    pub fn hash(&self, hasher: &impl HashProvider) -> Hash {
        match self {
            Self::Empty => hasher.hash(&[]),
            Self::Leaf(leaf) => leaf.hash,
            Self::Branch(branch) => branch.hash,
        }
    }

    /// Everything needed to reopen the root: nothing for an empty root,
    /// `hash ‖ commit` for a branch, `hash ‖ commit ‖ path` for a leaf.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Empty => Vec::new(),
            Self::Branch(branch) => [&branch.hash[..], &branch.commit.to_be_bytes()].concat(),
            Self::Leaf(leaf) => [&leaf.hash[..], &leaf.commit.to_be_bytes(), &leaf.path].concat(),
        }
    }

    /// Reverse [`RootRef::encode`]: empty bytes are the empty root, exactly
    /// [`BRANCH_ROOT_BYTES`] a branch, and `N` more bytes a leaf with its path.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::Empty);
        }
        let invalid = MerkleError::InvalidNode("invalid root encoding");
        let Some((hash, rest)) = bytes.split_first_chunk::<HASH_BYTES>() else {
            return Err(invalid);
        };
        let Some((commit, path)) = rest.split_first_chunk::<COMMIT_BYTES>() else {
            return Err(invalid);
        };
        let (hash, commit) = (*hash, u64::from_be_bytes(*commit));
        match path {
            [] => Ok(Self::Branch(BranchRef { hash, commit })),
            path => Ok(Self::Leaf(LeafRef {
                path: path.try_into().map_err(|_| invalid)?,
                hash,
                commit,
            })),
        }
    }
}

/// Branch-only compressed hexary trie over the caller's storage transaction.
/// Each table fixes a path width, domain, hash algorithm and canonical codec.
/// Branches are stored under `commit ‖ route` and never overwritten
/// by later commits. Every write names the commit that owns its new rows. Rows
/// rewritten within one commit replace each other, so only that commit's final
/// root stays valid.
/// No operation commits or opens a transaction. Abort the enclosing write
/// transaction after a mutation error: intermediate branches may be stored.
pub struct Trie<'h, H: HashProvider, const N: usize> {
    table: Table,
    domain: u8,
    hasher: &'h H,
}

impl<'h, H: HashProvider, const N: usize> Trie<'h, H, N> {
    /// The owner assigns `domain`, the byte prefixed to every branch hash.
    /// Configure a trie. Path widths outside 1..=[`crate::MAX_PATH_BYTES`]
    /// bytes fail at compile time.
    ///
    /// ```compile_fail,E0080
    /// use golemdb_merkle::{Keccak256Hasher, Trie, TrieTable};
    /// use golemdb_storage::Table;
    /// const EXAMPLE_BRANCH_DOMAIN: u8 = 0x03;
    /// const BRANCHES: TrieTable = TrieTable::new(Table("Branches"));
    /// let _ = Trie::<_, 0>::new(BRANCHES, EXAMPLE_BRANCH_DOMAIN, &Keccak256Hasher);
    /// ```
    pub fn new(table: TrieTable, domain: u8, hasher: &'h H) -> Self {
        const { check_path_width::<N>() };
        Self {
            table: table.table(),
            domain,
            hasher,
        }
    }

    /// Row key of the branch at `route` (nibbles from the root) written at
    /// `commit`.
    pub fn node_key(&self, commit: u64, route: &[u8]) -> Vec<u8> {
        // A route has at most `N` bytes of nibbles, so its length fits the byte.
        const { assert!(NIBBLES_PER_BYTE * N <= u8::MAX as usize) };
        let position = [&[route.len() as u8][..], &path::pack(route)].concat();
        version_key(commit, &position)
    }

    /// Validate a branch root's row, hash and routing at depth zero without
    /// traversing descendants. Empty/leaf references have no branch to load;
    /// authenticating a leaf payload remains the owner's responsibility.
    pub fn validate_root(&self, tx: &impl ReadTransaction, root: RootRef<N>) -> Result<()> {
        if let RootRef::Branch(branch) = root {
            self.load(tx, branch, &[])?;
        }
        Ok(())
    }

    pub fn get(
        &self,
        tx: &impl ReadTransaction,
        mut root: RootRef<N>,
        path: &[u8; N],
    ) -> Result<Option<LeafRef<N>>> {
        let target = path::nibbles(path);
        let mut route = Vec::new();
        loop {
            match root {
                RootRef::Empty => return Ok(None),
                RootRef::Leaf(leaf) => return Ok((leaf.path == *path).then_some(leaf)),
                RootRef::Branch(branch) => {
                    let node = self.load(tx, branch, &route)?;
                    route.extend(node.unpack_prefix());
                    if !target.starts_with(&route) {
                        return Ok(None);
                    }
                    let slot = target[route.len()];
                    route.push(slot);
                    root = node.child(slot).unwrap_or(RootRef::Empty);
                }
            }
        }
    }

    /// Insert or replace the leaf hash at `path`.
    pub fn insert(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<N>,
        path: &[u8; N],
        hash: Hash,
    ) -> Result<RootRef<N>> {
        let leaf = LeafRef::new(*path, hash);
        self.edit(tx, commit, root, path, Some(leaf), &[])
    }

    /// Remove the leaf at `path`, if present, merging a branch left with one
    /// child into its parent. New rows belong to `commit`.
    pub fn remove(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<N>,
        path: &[u8; N],
    ) -> Result<RootRef<N>> {
        self.edit(tx, commit, root, path, None, &[])
    }

    /// Set the leaf hash at each path, or remove the path for `None`; the last
    /// edit of a path wins. Same root as one `insert`/`remove` per edit, but
    /// each touched branch is rebuilt and stored once, so no intermediate
    /// version of a branch is written.
    pub fn apply(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<N>,
        edits: impl IntoIterator<Item = ([u8; N], Option<Hash>)>,
    ) -> Result<RootRef<N>> {
        let edits = edits.into_iter().map(|(path, hash)| Edit {
            path,
            hash,
            expected: None,
        });
        self.apply_edits(tx, commit, root, edits.collect())
    }

    /// [`Self::apply`], also checking each path's leaf hash before the batch
    /// (`None` for absent) against the first expectation given for it, on the
    /// same traversal that applies the edits. A mismatch is
    /// [`MerkleError::LeafMismatch`]; abort the transaction then, as rows may
    /// already be written.
    pub fn apply_checked(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<N>,
        edits: impl IntoIterator<Item = CheckedEdit<N>>,
    ) -> Result<RootRef<N>> {
        let edits = edits.into_iter().map(|edit| Edit {
            path: edit.path,
            hash: edit.after,
            expected: Some(edit.before),
        });
        self.apply_edits(tx, commit, root, edits.collect())
    }

    fn apply_edits(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<N>,
        mut edits: Vec<Edit<N>>,
    ) -> Result<RootRef<N>> {
        // Stable sorting preserves input order for each path: the last edit wins.
        edits.sort_by_key(|edit| edit.path);
        edits.dedup_by(|later, earlier| {
            if later.path == earlier.path {
                // dedup_by keeps the earlier entry; copy the last edit into it,
                // and keep the first expectation, which is of the old trie.
                earlier.hash = later.hash;
                earlier.expected = earlier.expected.or(later.expected);
                true
            } else {
                false
            }
        });
        let edited = self.apply_at(tx, commit, Edited::Stored(root), &edits, &[])?;
        self.store_edited(tx, commit, edited, &[])
    }

    /// Lazy ascending path traversal. Storage/corruption errors are yielded once
    /// and terminate iteration. Only branches on the traversal frontier are held.
    pub fn walk<'a, T: ReadTransaction>(
        &'a self,
        tx: &'a T,
        root: RootRef<N>,
    ) -> Walk<'a, T, H, N> {
        Walk {
            trie: self,
            tx,
            stack: vec![(root, Vec::new())],
        }
    }

    fn load(
        &self,
        tx: &impl ReadTransaction,
        branch: BranchRef,
        route: &[u8],
    ) -> Result<BranchNodeCompact<N>> {
        let bytes = tx
            .get(self.table, &self.node_key(branch.commit, route))?
            .ok_or(MerkleError::MissingBranch(branch.hash))?;
        let node = BranchNodeCompact::decode(&bytes)?;
        if node.hash(self.domain, self.hasher) != branch.hash {
            return Err(MerkleError::HashMismatch);
        }
        node.validate_at(route)?;
        Ok(node)
    }

    /// Store the branch over `children` with `prefix` at `route`.
    fn store(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        prefix: &[u8],
        children: &[RootRef<N>; BRANCH_SLOTS],
        route: &[u8],
    ) -> Result<RootRef<N>> {
        let node = BranchNodeCompact::from_children(prefix, children)?;
        node.validate_at(route)?;
        let hash = node.hash(self.domain, self.hasher);
        tx.put(self.table, &self.node_key(commit, route), &node.encode())?;
        Ok(RootRef::Branch(BranchRef { hash, commit }))
    }

    fn edit(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<N>,
        target: &[u8; N],
        replacement: Option<LeafRef<N>>,
        route: &[u8],
    ) -> Result<RootRef<N>> {
        match root {
            RootRef::Empty => Ok(replacement.map(RootRef::Leaf).unwrap_or(RootRef::Empty)),
            RootRef::Leaf(leaf) => self.edit_leaf(tx, commit, leaf, target, replacement, route),
            RootRef::Branch(branch) => {
                self.edit_branch(tx, commit, branch, target, replacement, route)
            }
        }
    }

    /// Replace/delete a matching leaf, or split distinct leaves on insertion.
    fn edit_leaf(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        old: LeafRef<N>,
        target: &[u8; N],
        replacement: Option<LeafRef<N>>,
        route: &[u8],
    ) -> Result<RootRef<N>> {
        if old.path == *target {
            return Ok(replacement.map(RootRef::Leaf).unwrap_or(RootRef::Empty));
        }
        let Some(new) = replacement else {
            return Ok(RootRef::Leaf(old));
        };
        let old_path = path::nibbles(&old.path);
        let new_path = path::nibbles(target);
        let depth = (route.len()..N * NIBBLES_PER_BYTE)
            .find(|&i| old_path[i] != new_path[i])
            .unwrap();
        let mut children = [RootRef::Empty; BRANCH_SLOTS];
        children[old_path[depth] as usize] = RootRef::Leaf(old);
        children[new_path[depth] as usize] = RootRef::Leaf(new);
        self.store(tx, commit, &new_path[route.len()..depth], &children, route)
    }

    /// Split a divergent prefix or recursively edit the matching child.
    fn edit_branch(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        branch: BranchRef,
        target: &[u8; N],
        replacement: Option<LeafRef<N>>,
        route: &[u8],
    ) -> Result<RootRef<N>> {
        let node = self.load(tx, branch, route)?;
        let prefix = node.unpack_prefix();
        let target_path = path::nibbles(target);
        let shared = prefix
            .iter()
            .zip(&target_path[route.len()..])
            .take_while(|(a, b)| a == b)
            .count();
        if shared != prefix.len() {
            let Some(new) = replacement else {
                return Ok(RootRef::Branch(branch));
            };
            return self.split_branch(tx, commit, &node, new, shared, route);
        }
        let mut child_route = route.to_vec();
        child_route.extend_from_slice(&prefix);
        let slot = target_path[child_route.len()] as usize;
        child_route.push(slot as u8);
        let mut children = node.children();
        let changed = self.edit(
            tx,
            commit,
            children[slot],
            target,
            replacement,
            &child_route,
        )?;
        if changed == children[slot] {
            return Ok(RootRef::Branch(branch));
        }
        children[slot] = changed;
        self.rebuild_branch(tx, commit, prefix, &children, route)
    }

    /// Insert a leaf diverging within this branch's prefix. Store the shortened
    /// old branch first, then a parent joining it with the new leaf.
    fn split_branch(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        node: &BranchNodeCompact<N>,
        new: LeafRef<N>,
        shared: usize,
        route: &[u8],
    ) -> Result<RootRef<N>> {
        let prefix = node.unpack_prefix();
        let mut old_route = route.to_vec();
        old_route.extend_from_slice(&prefix[..shared + 1]);
        let old = self.store(
            tx,
            commit,
            &prefix[shared + 1..],
            &node.children(),
            &old_route,
        )?;
        let mut children = [RootRef::Empty; BRANCH_SLOTS];
        children[prefix[shared] as usize] = old;
        children[path::nibble(&new.path, route.len() + shared) as usize] = RootRef::Leaf(new);
        self.store(tx, commit, &prefix[..shared], &children, route)
    }

    /// Store updated children, promoting a lone leaf or merging a lone branch's
    /// prefix so the trie never persists unary branches.
    fn rebuild_branch(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        prefix: Vec<u8>,
        children: &[RootRef<N>; BRANCH_SLOTS],
        route: &[u8],
    ) -> Result<RootRef<N>> {
        let active: Vec<_> = children
            .iter()
            .enumerate()
            .filter(|(_, child)| **child != RootRef::Empty)
            .collect();
        if let [(slot, only)] = active.as_slice() {
            if let RootRef::Branch(branch) = **only {
                let mut child_route = route.to_vec();
                child_route.extend_from_slice(&prefix);
                child_route.push(*slot as u8);
                let child = self.load(tx, branch, &child_route)?;
                let mut merged = prefix;
                merged.push(*slot as u8);
                merged.extend(child.unpack_prefix());
                self.store(tx, commit, &merged, &child.children(), route)
            } else {
                Ok(**only)
            }
        } else {
            self.store(tx, commit, &prefix, children, route)
        }
    }

    /// Edits are sorted by path, unique, and share `route`. A branch keeps its
    /// untouched children. An insertion off its prefix splits it as
    /// `split_branch` does, so the work stays proportional to the edited paths;
    /// a removal off its prefix has nothing to remove.
    fn apply_at(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        subtrie: Edited<N>,
        edits: &[Edit<N>],
        route: &[u8],
    ) -> Result<Edited<N>> {
        let (stored, mut prefix, children) = match subtrie {
            Edited::Stored(RootRef::Empty) => {
                check(edits, None)?;
                return self.rebuild(tx, commit, None, edits, route);
            }
            Edited::Stored(RootRef::Leaf(leaf)) => {
                check(edits, Some(leaf))?;
                return self.rebuild(tx, commit, Some(leaf), edits, route);
            }
            Edited::Stored(RootRef::Branch(branch)) => {
                let node = self.load(tx, branch, route)?;
                (Some(branch), node.unpack_prefix(), node.children())
            }
            Edited::New(prefix, children) => (None, prefix, *children),
        };
        let (kept, mut edited_children) = split_prefix(&prefix, children, edits, route.len());
        let inner = following(edits, &prefix[..kept], route.len());
        // The rest leave the old branch's prefix, so they are absent from it.
        check(&edits[..inner.start], None)?;
        check(&edits[inner.end..], None)?;
        let edits = &edits[inner];
        let was_split = kept < prefix.len();
        prefix.truncate(kept);
        let mut child_route = [route, &prefix].concat();
        let depth = child_route.len();
        for group in
            edits.chunk_by(|a, b| path::nibble(&a.path, depth) == path::nibble(&b.path, depth))
        {
            let slot = path::nibble(&group[0].path, depth) as usize;
            child_route.push(slot as u8);
            let child =
                std::mem::replace(&mut edited_children[slot], Edited::Stored(RootRef::Empty));
            edited_children[slot] = self.apply_at(tx, commit, child, group, &child_route)?;
            child_route.pop();
        }
        let unchanged = !was_split
            && (edited_children.iter().zip(children))
                .all(|(edited, child)| *edited == Edited::Stored(child));
        if unchanged {
            return Ok(match stored {
                Some(branch) => Edited::Stored(RootRef::Branch(branch)),
                None => Edited::New(prefix, Box::new(children)),
            });
        }
        self.collapse(tx, commit, prefix, edited_children, child_route)
    }

    /// Form the edited branch from its edited children, keeping the trie free
    /// of unary branches: with no children it is empty, a lone leaf replaces
    /// it, and a lone branch absorbs its prefix. Several children are stored
    /// under it. `child_route` is the branch's route followed by its prefix.
    fn collapse(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        prefix: Vec<u8>,
        mut edited_children: [Edited<N>; BRANCH_SLOTS],
        mut child_route: Vec<u8>,
    ) -> Result<Edited<N>> {
        let active: Vec<_> = (0..BRANCH_SLOTS)
            .filter(|&slot| edited_children[slot] != Edited::Stored(RootRef::Empty))
            .collect();
        match active[..] {
            [] => Ok(Edited::Stored(RootRef::Empty)),
            [slot] => {
                let only =
                    std::mem::replace(&mut edited_children[slot], Edited::Stored(RootRef::Empty));
                let (child_prefix, grandchildren) = match only {
                    Edited::New(child_prefix, grandchildren) => (child_prefix, grandchildren),
                    Edited::Stored(RootRef::Branch(branch)) => {
                        child_route.push(slot as u8);
                        let child = self.load(tx, branch, &child_route)?;
                        (child.unpack_prefix(), Box::new(child.children()))
                    }
                    leaf => return Ok(leaf),
                };
                let merged = [&prefix[..], &[slot as u8], &child_prefix].concat();
                Ok(Edited::New(merged, grandchildren))
            }
            _ => {
                let mut children = [RootRef::Empty; BRANCH_SLOTS];
                for (slot, edited) in edited_children.into_iter().enumerate() {
                    child_route.push(slot as u8);
                    children[slot] = self.store_edited(tx, commit, edited, &child_route)?;
                    child_route.pop();
                }
                Ok(Edited::New(prefix, Box::new(children)))
            }
        }
    }

    /// Build the subtrie for an empty or single-leaf subtrie, `old`, merged
    /// with `edits`. The old leaf survives unless an edit replaces or removes it.
    fn rebuild(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        old: Option<LeafRef<N>>,
        edits: &[Edit<N>],
        route: &[u8],
    ) -> Result<Edited<N>> {
        let mut leaves = Vec::new();
        for &Edit { path, hash, .. } in edits {
            let Some(hash) = hash else { continue };
            leaves.push(match old {
                Some(old) if old.path == path && old.hash == hash => old,
                _ => LeafRef::new(path, hash),
            });
        }
        if let Some(old) = old.filter(|old| edits.iter().all(|edit| edit.path != old.path)) {
            leaves.push(old);
            leaves.sort_by_key(|leaf| leaf.path);
        }
        self.build(tx, commit, &leaves, route)
    }

    /// The canonical subtrie at `route` for leaves sorted by path.
    fn build(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        leaves: &[LeafRef<N>],
        route: &[u8],
    ) -> Result<Edited<N>> {
        let (first, last) = match leaves {
            [] => return Ok(Edited::Stored(RootRef::Empty)),
            [only] => return Ok(Edited::Stored(RootRef::Leaf(*only))),
            [first, .., last] => (path::nibbles(&first.path), path::nibbles(&last.path)),
        };
        let depth = (route.len()..N * NIBBLES_PER_BYTE)
            .find(|&i| first[i] != last[i])
            .unwrap();
        let mut children = [RootRef::Empty; BRANCH_SLOTS];
        let mut child_route = first[..depth].to_vec();
        for group in
            leaves.chunk_by(|a, b| path::nibble(&a.path, depth) == path::nibble(&b.path, depth))
        {
            child_route.push(path::nibble(&group[0].path, depth));
            let child = self.build(tx, commit, group, &child_route)?;
            children[child_route[depth] as usize] =
                self.store_edited(tx, commit, child, &child_route)?;
            child_route.pop();
        }
        Ok(Edited::New(
            first[route.len()..depth].to_vec(),
            Box::new(children),
        ))
    }

    /// Store a new branch at `route` and return its reference. Anything else
    /// is already stored, or is empty or a leaf, which have no row.
    fn store_edited(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        edited: Edited<N>,
        route: &[u8],
    ) -> Result<RootRef<N>> {
        match edited {
            Edited::Stored(root) => Ok(root),
            Edited::New(prefix, children) => self.store(tx, commit, &prefix, &children, route),
        }
    }
}

/// A subtrie edited by a batch. A new branch is stored only once its parent
/// keeps it beside a sibling; a parent left with one child absorbs it instead.
/// A branch moved below a split prefix is also new until it is stored.
#[derive(PartialEq)]
enum Edited<const N: usize> {
    Stored(RootRef<N>),
    New(Vec<u8>, Box<[RootRef<N>; BRANCH_SLOTS]>),
}

/// An edit for [`Trie::apply_checked`]: the leaf hash expected at `path`
/// before the batch and the one to set after it, `None` for absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckedEdit<const N: usize> {
    pub path: [u8; N],
    pub before: Option<Hash>,
    pub after: Option<Hash>,
}

/// One batch edit: the leaf hash to set at a path, or `None` to remove it,
/// and the hash expected there before the batch, when checked.
#[derive(Clone, Copy)]
struct Edit<const N: usize> {
    path: [u8; N],
    hash: Option<Hash>,
    expected: Option<Option<Hash>>,
}

/// Check the edits' expectations against `old`, the only leaf their subtrie
/// holds, if any.
fn check<const N: usize>(edits: &[Edit<N>], old: Option<LeafRef<N>>) -> Result<()> {
    for edit in edits {
        let actual = old.filter(|old| old.path == edit.path).map(|old| old.hash);
        if edit.expected.is_some_and(|expected| expected != actual) {
            return Err(MerkleError::LeafMismatch);
        }
    }
    Ok(())
}

/// Split a branch's `prefix`, whose route has `depth` nibbles, at the first
/// nibble an insertion leaves it. Returns how many prefix nibbles the branch
/// keeps, and its children: after a split, the old branch alone, in slot
/// `prefix[kept]` with the rest of its prefix and its own children; otherwise
/// its own children unchanged.
fn split_prefix<const N: usize>(
    prefix: &[u8],
    children: [RootRef<N>; BRANCH_SLOTS],
    edits: &[Edit<N>],
    depth: usize,
) -> (usize, [Edited<N>; BRANCH_SLOTS]) {
    let kept = (edits.iter())
        .filter(|edit| edit.hash.is_some())
        .map(|edit| path::matched(prefix, &edit.path, depth))
        .min()
        .unwrap_or(prefix.len());
    if kept == prefix.len() {
        return (kept, children.map(Edited::Stored));
    }
    let mut split: [Edited<N>; BRANCH_SLOTS] =
        std::array::from_fn(|_| Edited::Stored(RootRef::Empty));
    split[prefix[kept] as usize] = Edited::New(prefix[kept + 1..].to_vec(), Box::new(children));
    (kept, split)
}

/// The positions of the edits whose paths follow `prefix` from nibble
/// `depth`. Sorted paths that share a prefix are contiguous, so they form one
/// range.
fn following<const N: usize>(
    edits: &[Edit<N>],
    prefix: &[u8],
    depth: usize,
) -> std::ops::Range<usize> {
    let follows = |edit: &Edit<N>| path::matched(prefix, &edit.path, depth) == prefix.len();
    let start = edits.iter().position(follows).unwrap_or(edits.len());
    let end = edits
        .iter()
        .rposition(follows)
        .map_or(start, |last| last + 1);
    start..end
}

pub struct Walk<'a, T: ReadTransaction, H: HashProvider, const N: usize> {
    trie: &'a Trie<'a, H, N>,
    tx: &'a T,
    stack: Vec<(RootRef<N>, Vec<u8>)>,
}

impl<T: ReadTransaction, H: HashProvider, const N: usize> Iterator for Walk<'_, T, H, N> {
    type Item = Result<LeafRef<N>>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((root, mut route)) = self.stack.pop() {
            match root {
                RootRef::Empty => (),
                RootRef::Leaf(leaf) => return Some(Ok(leaf)),
                RootRef::Branch(branch) => {
                    let node = match self.trie.load(self.tx, branch, &route) {
                        Ok(node) => node,
                        Err(error) => {
                            self.stack.clear();
                            return Some(Err(error));
                        }
                    };
                    route.extend(node.unpack_prefix());
                    for slot in (0..BRANCH_SLOTS as u8).rev() {
                        if let Some(child) = node.child(slot) {
                            let mut child_route = route.clone();
                            child_route.push(slot);
                            self.stack.push((child, child_route));
                        }
                    }
                }
            }
        }
        None
    }
}

impl<T: ReadTransaction, H: HashProvider, const N: usize> std::iter::FusedIterator
    for Walk<'_, T, H, N>
{
}
