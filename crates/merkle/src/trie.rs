use crate::{BranchNodeCompact, Hash, HashProvider, MerkleError, Result, path};
use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

/// The owner supplies a hash binding the complete path and its leaf payload.
/// `N` is the full routing path width in bytes, not the trie depth.
/// `commit` locates a leaf stored as its own row ([`Trie::with_leaf_rows`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeafRef<const N: usize> {
    pub path: [u8; N],
    pub hash: Hash,
    pub commit: u64,
}

impl<const N: usize> LeafRef<N> {
    /// A leaf without a row of its own.
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

/// Row key: the writing commit first, so each commit's rows are contiguous.
pub fn version_key(commit: u64, namespace: &[u8], position: &[u8]) -> Vec<u8> {
    [&commit.to_be_bytes()[..], namespace, position].concat()
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
    /// Empty root = H(empty bytes); singleton root = its leaf hash.
    pub fn hash(&self, hasher: &impl HashProvider) -> Hash {
        match self {
            Self::Empty => hasher.hash(&[]),
            Self::Leaf(leaf) => leaf.hash,
            Self::Branch(branch) => branch.hash,
        }
    }

    /// The commit that stored a branch root; other roots have no row.
    pub fn branch_commit(&self) -> Option<u64> {
        match self {
            Self::Branch(branch) => Some(branch.commit),
            _ => None,
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

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::Empty);
        }
        let invalid = MerkleError::InvalidNode("invalid root encoding");
        let (Some(hash), Some(commit)) = (bytes.get(..32), bytes.get(32..40)) else {
            return Err(invalid);
        };
        let hash = hash.try_into().unwrap();
        let commit = u64::from_be_bytes(commit.try_into().unwrap());
        match &bytes[40..] {
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
/// Branches are stored under `commit ‖ namespace ‖ route` and never overwritten
/// by later commits; one batch writes each route at most once per commit.
/// No operation commits or opens a transaction. Abort the enclosing write
/// transaction after a mutation error: intermediate branches may be stored.
pub struct Trie<'h, H: HashProvider, const N: usize> {
    table: Table,
    domain: u8,
    hasher: &'h H,
    commit: u64,
    namespace: Vec<u8>,
    leaf_rows: bool,
}

impl<'h, H: HashProvider, const N: usize> Trie<'h, H, N> {
    /// The owner assigns `domain`, the byte prefixed to every branch hash.
    /// Configure a trie. Path widths outside 1..=40 bytes fail at compile time.
    ///
    /// ```compile_fail,E0080
    /// use golemdb_merkle::{Keccak256Hasher, Trie};
    /// use golemdb_storage::Table;
    /// const EXAMPLE_BRANCH_DOMAIN: u8 = 0x03;
    /// let _ = Trie::<_, 0>::new(Table("Branches"), EXAMPLE_BRANCH_DOMAIN, &Keccak256Hasher);
    /// ```
    ///
    /// ```compile_fail,E0080
    /// use golemdb_merkle::{Keccak256Hasher, Trie};
    /// use golemdb_storage::Table;
    /// const EXAMPLE_BRANCH_DOMAIN: u8 = 0x03;
    /// let _ = Trie::<_, 41>::new(Table("Branches"), EXAMPLE_BRANCH_DOMAIN, &Keccak256Hasher);
    /// ```
    pub fn new(table: Table, domain: u8, hasher: &'h H) -> Self {
        const {
            assert!(
                N >= 1 && N <= 40,
                "Merkle paths must contain between 1 and 40 bytes"
            )
        };
        Self {
            table,
            domain,
            hasher,
            commit: 0,
            namespace: Vec::new(),
            leaf_rows: false,
        }
    }

    /// Store new rows at `commit`.
    pub fn at(mut self, commit: u64) -> Self {
        self.commit = commit;
        self
    }

    /// Key rows under `namespace`, for several tries sharing one table.
    pub fn in_namespace(mut self, namespace: &[u8]) -> Self {
        self.namespace = namespace.to_vec();
        self
    }

    /// Leaves are rows of their own, so branches record their commits too.
    pub fn with_leaf_rows(mut self) -> Self {
        self.leaf_rows = true;
        self
    }

    pub(crate) fn node_key(&self, commit: u64, route: &[u8]) -> Vec<u8> {
        let position = [&[route.len() as u8][..], &path::pack(route)].concat();
        version_key(commit, &self.namespace, &position)
    }

    fn leaf(&self, path: [u8; N], hash: Hash) -> LeafRef<N> {
        LeafRef {
            path,
            hash,
            commit: if self.leaf_rows { self.commit } else { 0 },
        }
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

    pub fn insert(
        &self,
        tx: &mut impl WriteTransaction,
        root: RootRef<N>,
        leaf: LeafRef<N>,
    ) -> Result<RootRef<N>> {
        self.edit(tx, root, &leaf.path, Some(leaf), &[])
    }

    pub fn remove(
        &self,
        tx: &mut impl WriteTransaction,
        root: RootRef<N>,
        path: &[u8; N],
    ) -> Result<RootRef<N>> {
        self.edit(tx, root, path, None, &[])
    }

    /// Set the leaf hash at each path, or remove the path for `None`; the last
    /// edit of a path wins. Same root as one `insert`/`remove` per edit, but
    /// each touched branch is rebuilt and stored once, so no intermediate
    /// version of a branch is written.
    pub fn apply(
        &self,
        tx: &mut impl WriteTransaction,
        root: RootRef<N>,
        edits: impl IntoIterator<Item = ([u8; N], Option<Hash>)>,
    ) -> Result<RootRef<N>> {
        let mut edits: Vec<_> = edits.into_iter().collect();
        // Stable sorting preserves input order for each path: the last edit wins.
        edits.sort_by_key(|edit| edit.0);
        edits.dedup_by(|later, earlier| {
            if later.0 == earlier.0 {
                // dedup_by keeps the earlier entry; copy the last edit into it.
                earlier.1 = later.1;
                true
            } else {
                false
            }
        });
        let edited = self.apply_at(tx, Edited::Stored(root), &edits, &[])?;
        self.store_edited(tx, edited, &[])
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
        if node.has_leaf_commits() != (self.leaf_rows && node.has_leaves()) {
            return Err(MerkleError::InvalidNode(
                "leaf commits disagree with the trie",
            ));
        }
        node.validate_at(route)?;
        Ok(node)
    }

    fn store(
        &self,
        tx: &mut impl WriteTransaction,
        node: BranchNodeCompact<N>,
        route: &[u8],
    ) -> Result<RootRef<N>> {
        node.validate_at(route)?;
        let hash = node.hash(self.domain, self.hasher);
        tx.put(
            self.table,
            &self.node_key(self.commit, route),
            &node.encode(),
        )?;
        Ok(RootRef::Branch(BranchRef {
            hash,
            commit: self.commit,
        }))
    }

    fn edit(
        &self,
        tx: &mut impl WriteTransaction,
        root: RootRef<N>,
        target: &[u8; N],
        replacement: Option<LeafRef<N>>,
        route: &[u8],
    ) -> Result<RootRef<N>> {
        match root {
            RootRef::Empty => Ok(replacement.map(RootRef::Leaf).unwrap_or(RootRef::Empty)),
            RootRef::Leaf(leaf) => self.edit_leaf(tx, leaf, target, replacement, route),
            RootRef::Branch(branch) => self.edit_branch(tx, branch, target, replacement, route),
        }
    }

    /// Replace/delete a matching leaf, or split distinct leaves on insertion.
    fn edit_leaf(
        &self,
        tx: &mut impl WriteTransaction,
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
        let depth = (route.len()..N * 2)
            .find(|&i| old_path[i] != new_path[i])
            .unwrap();
        let mut children = [RootRef::Empty; 16];
        children[old_path[depth] as usize] = RootRef::Leaf(old);
        children[new_path[depth] as usize] = RootRef::Leaf(new);
        self.store(
            tx,
            BranchNodeCompact::from_children(
                &new_path[route.len()..depth],
                &children,
                self.leaf_rows,
            )?,
            route,
        )
    }

    /// Split a divergent prefix or recursively edit the matching child.
    fn edit_branch(
        &self,
        tx: &mut impl WriteTransaction,
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
            return self.split_branch(tx, &node, &prefix, new, shared, route);
        }
        let mut child_route = route.to_vec();
        child_route.extend_from_slice(&prefix);
        let slot = target_path[child_route.len()] as usize;
        child_route.push(slot as u8);
        let mut children = node.children();
        let changed = self.edit(tx, children[slot], target, replacement, &child_route)?;
        if changed == children[slot] {
            return Ok(RootRef::Branch(branch));
        }
        children[slot] = changed;
        self.rebuild_branch(tx, prefix, &children, route)
    }

    /// Insert a leaf diverging within this branch's prefix. Store the shortened
    /// old branch first, then a parent joining it with the new leaf.
    fn split_branch(
        &self,
        tx: &mut impl WriteTransaction,
        node: &BranchNodeCompact<N>,
        prefix: &[u8],
        new: LeafRef<N>,
        shared: usize,
        route: &[u8],
    ) -> Result<RootRef<N>> {
        let mut old_route = route.to_vec();
        old_route.extend_from_slice(&prefix[..shared + 1]);
        let old = self.store(
            tx,
            BranchNodeCompact::from_children(
                &prefix[shared + 1..],
                &node.children(),
                self.leaf_rows,
            )?,
            &old_route,
        )?;
        let mut children = [RootRef::Empty; 16];
        children[prefix[shared] as usize] = old;
        children[path::nibble(&new.path, route.len() + shared) as usize] = RootRef::Leaf(new);
        self.store(
            tx,
            BranchNodeCompact::from_children(&prefix[..shared], &children, self.leaf_rows)?,
            route,
        )
    }

    /// Store updated children, promoting a lone leaf or merging a lone branch's
    /// prefix so the trie never persists unary branches.
    fn rebuild_branch(
        &self,
        tx: &mut impl WriteTransaction,
        prefix: Vec<u8>,
        children: &[RootRef<N>; 16],
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
                self.store(
                    tx,
                    BranchNodeCompact::from_children(&merged, &child.children(), self.leaf_rows)?,
                    route,
                )
            } else {
                Ok(**only)
            }
        } else {
            self.store(
                tx,
                BranchNodeCompact::from_children(&prefix, children, self.leaf_rows)?,
                route,
            )
        }
    }

    /// Edits are sorted by path, unique, and share `route`. A branch keeps its
    /// untouched children. An insertion off its prefix splits it as
    /// `split_branch` does, so the work stays proportional to the edited paths;
    /// a removal off its prefix has nothing to remove.
    fn apply_at(
        &self,
        tx: &mut impl WriteTransaction,
        subtrie: Edited<N>,
        edits: &[Edit<N>],
        route: &[u8],
    ) -> Result<Edited<N>> {
        let (stored, mut prefix, children) = match subtrie {
            Edited::Stored(RootRef::Empty) => return self.rebuild(tx, None, edits, route),
            Edited::Stored(RootRef::Leaf(leaf)) => {
                return self.rebuild(tx, Some(leaf), edits, route);
            }
            Edited::Stored(RootRef::Branch(branch)) => {
                let node = self.load(tx, branch, route)?;
                (Some(branch), node.unpack_prefix(), node.children())
            }
            Edited::New(prefix, children) => (None, prefix, *children),
        };
        let (kept, mut edited_children) = split_prefix(&prefix, children, edits, route.len());
        let edits = following(edits, &prefix[..kept], route.len());
        let was_split = kept < prefix.len();
        prefix.truncate(kept);
        let mut child_route = [route, &prefix].concat();
        let depth = child_route.len();
        for group in edits.chunk_by(|a, b| path::nibble(&a.0, depth) == path::nibble(&b.0, depth)) {
            let slot = path::nibble(&group[0].0, depth) as usize;
            child_route.push(slot as u8);
            let child =
                std::mem::replace(&mut edited_children[slot], Edited::Stored(RootRef::Empty));
            edited_children[slot] = self.apply_at(tx, child, group, &child_route)?;
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
        self.collapse(tx, prefix, edited_children, child_route)
    }

    /// Form the edited branch from its edited children, keeping the trie free
    /// of unary branches: with no children it is empty, a lone leaf replaces
    /// it, and a lone branch absorbs its prefix. Several children are stored
    /// under it. `child_route` is the branch's route followed by its prefix.
    fn collapse(
        &self,
        tx: &mut impl WriteTransaction,
        prefix: Vec<u8>,
        mut edited_children: [Edited<N>; 16],
        mut child_route: Vec<u8>,
    ) -> Result<Edited<N>> {
        let active: Vec<_> = (0..16)
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
                let mut children = [RootRef::Empty; 16];
                for (slot, edited) in edited_children.into_iter().enumerate() {
                    child_route.push(slot as u8);
                    children[slot] = self.store_edited(tx, edited, &child_route)?;
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
        old: Option<LeafRef<N>>,
        edits: &[Edit<N>],
        route: &[u8],
    ) -> Result<Edited<N>> {
        let mut leaves: Vec<_> = (edits.iter())
            .filter_map(|&(path, hash)| hash.map(|hash| self.leaf(path, hash)))
            .collect();
        if let Some(old) = old.filter(|old| edits.iter().all(|(path, _)| *path != old.path)) {
            leaves.push(old);
            leaves.sort_by_key(|leaf| leaf.path);
        }
        self.build(tx, &leaves, route)
    }

    /// The canonical subtrie at `route` for leaves sorted by path.
    fn build(
        &self,
        tx: &mut impl WriteTransaction,
        leaves: &[LeafRef<N>],
        route: &[u8],
    ) -> Result<Edited<N>> {
        let (first, last) = match leaves {
            [] => return Ok(Edited::Stored(RootRef::Empty)),
            [only] => return Ok(Edited::Stored(RootRef::Leaf(*only))),
            [first, .., last] => (path::nibbles(&first.path), path::nibbles(&last.path)),
        };
        let depth = (route.len()..N * 2).find(|&i| first[i] != last[i]).unwrap();
        let mut children = [RootRef::Empty; 16];
        let mut child_route = first[..depth].to_vec();
        for group in
            leaves.chunk_by(|a, b| path::nibble(&a.path, depth) == path::nibble(&b.path, depth))
        {
            child_route.push(path::nibble(&group[0].path, depth));
            let child = self.build(tx, group, &child_route)?;
            children[child_route[depth] as usize] = self.store_edited(tx, child, &child_route)?;
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
        edited: Edited<N>,
        route: &[u8],
    ) -> Result<RootRef<N>> {
        match edited {
            Edited::Stored(root) => Ok(root),
            Edited::New(prefix, children) => self.store(
                tx,
                BranchNodeCompact::from_children(&prefix, &children, self.leaf_rows)?,
                route,
            ),
        }
    }
}

/// A subtrie edited by a batch. A new branch is stored only once its parent
/// keeps it beside a sibling; a parent left with one child absorbs it instead.
/// A branch moved below a split prefix is also new until it is stored.
#[derive(PartialEq)]
enum Edited<const N: usize> {
    Stored(RootRef<N>),
    New(Vec<u8>, Box<[RootRef<N>; 16]>),
}

/// One batch edit: the leaf hash to set at a path, or `None` to remove it.
type Edit<const N: usize> = ([u8; N], Option<Hash>);

/// Split a branch's `prefix`, whose route has `depth` nibbles, at the first
/// nibble an insertion leaves it. Returns how many prefix nibbles the branch
/// keeps, and its children: after a split, the old branch alone, in slot
/// `prefix[kept]` with the rest of its prefix and its own children; otherwise
/// its own children unchanged.
fn split_prefix<const N: usize>(
    prefix: &[u8],
    children: [RootRef<N>; 16],
    edits: &[Edit<N>],
    depth: usize,
) -> (usize, [Edited<N>; 16]) {
    let kept = (edits.iter())
        .filter(|(_, hash)| hash.is_some())
        .map(|(path, _)| path::matched(prefix, path, depth))
        .min()
        .unwrap_or(prefix.len());
    if kept == prefix.len() {
        return (kept, children.map(Edited::Stored));
    }
    let mut split: [Edited<N>; 16] = std::array::from_fn(|_| Edited::Stored(RootRef::Empty));
    split[prefix[kept] as usize] = Edited::New(prefix[kept + 1..].to_vec(), Box::new(children));
    (kept, split)
}

/// The edits whose paths follow `prefix` from nibble `depth`. Sorted paths
/// that share a prefix are contiguous, so they form one subslice.
fn following<'e, const N: usize>(
    edits: &'e [Edit<N>],
    prefix: &[u8],
    depth: usize,
) -> &'e [Edit<N>] {
    let follows = |(path, _): &Edit<N>| path::matched(prefix, path, depth) == prefix.len();
    let start = edits.iter().position(follows).unwrap_or(edits.len());
    let end = edits
        .iter()
        .rposition(follows)
        .map_or(start, |last| last + 1);
    &edits[start..end]
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
                    for slot in (0..16).rev() {
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
