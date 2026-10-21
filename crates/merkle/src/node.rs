use crate::{BranchRef, Hash, HashProvider, LeafRef, MerkleError, Result, RootRef, path};

/// Where a trie keeps its leaves, and so which children a branch records the
/// commit of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LeafStorage {
    /// Leaves are only entries in their parent branch; branch children have rows.
    #[default]
    InNode,
    /// Every leaf is also a row of its own ([`crate::PayloadTrie`]).
    Rows,
}

impl LeafStorage {
    /// The children whose rows a branch locates.
    fn stored_mask(self, state_mask: u16, tree_mask: u16) -> u16 {
        match self {
            Self::InNode => tree_mask,
            Self::Rows => state_mask,
        }
    }
}

/// Canonical compact branch. Leaf paths and child commits are stored routing
/// metadata and are excluded from the hash; the owner must bind each full path
/// in its leaf hash. Child commits locate each branch child's row, and each
/// leaf child's row when leaves are rows of their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchNodeCompact<const N: usize> {
    prefix: Vec<u8>,
    prefix_len: u8,
    state_mask: u16,
    tree_mask: u16,
    child_hashes: Vec<Hash>,
    leaf_paths: Vec<[u8; N]>,
    child_commits: Vec<u64>,
    leaf_storage: LeafStorage,
}

impl<const N: usize> BranchNodeCompact<N> {
    /// `prefix` is packed high nibble first, relative to this branch's depth.
    /// Hashes, leaf paths and commits follow ascending occupied slots in their
    /// masks; there is one commit per branch child, or per child with
    /// [`LeafStorage::Rows`]. Path widths outside 1..=32 bytes fail at compile time;
    /// malformed branch contents still return runtime errors.
    ///
    /// ```compile_fail,E0080
    /// use golemdb_merkle::BranchNodeCompact;
    /// use golemdb_merkle::LeafStorage;
    /// let _ = BranchNodeCompact::<33>::new(
    ///     vec![], 0, 3, 3, vec![[0; 32]; 2], vec![], vec![0; 2], LeafStorage::InNode,
    /// );
    /// ```
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        prefix: Vec<u8>,
        prefix_len: u8,
        state_mask: u16,
        tree_mask: u16,
        child_hashes: Vec<Hash>,
        leaf_paths: Vec<[u8; N]>,
        child_commits: Vec<u64>,
        leaf_storage: LeafStorage,
    ) -> Result<Self> {
        const {
            assert!(
                N >= 1 && N <= 32,
                "Merkle paths must contain between 1 and 32 bytes"
            )
        };
        if prefix_len as usize >= 2 * N || prefix.len() != (prefix_len as usize).div_ceil(2) {
            return Err(MerkleError::InvalidNode("invalid prefix length"));
        }
        if prefix_len % 2 == 1 && prefix.last().is_some_and(|byte| byte & 15 != 0) {
            return Err(MerkleError::InvalidNode("nonzero prefix padding"));
        }
        if state_mask.count_ones() < 2
            || tree_mask & !state_mask != 0
            || child_hashes.len() != state_mask.count_ones() as usize
            || leaf_paths.len() != (state_mask & !tree_mask).count_ones() as usize
            || child_commits.len()
                != leaf_storage.stored_mask(state_mask, tree_mask).count_ones() as usize
        {
            return Err(MerkleError::InvalidNode("invalid child masks or counts"));
        }
        Ok(Self {
            prefix,
            prefix_len,
            state_mask,
            tree_mask,
            child_hashes,
            leaf_paths,
            child_commits,
            leaf_storage,
        })
    }

    pub fn prefix(&self) -> &[u8] {
        &self.prefix
    }
    pub fn prefix_len(&self) -> u8 {
        self.prefix_len
    }
    pub fn state_mask(&self) -> u16 {
        self.state_mask
    }
    pub fn tree_mask(&self) -> u16 {
        self.tree_mask
    }
    pub fn child_hashes(&self) -> &[Hash] {
        &self.child_hashes
    }
    pub fn leaf_paths(&self) -> &[[u8; N]] {
        &self.leaf_paths
    }

    pub fn child(&self, slot: u8) -> Option<RootRef<N>> {
        if slot >= 16 || self.state_mask & (1 << slot) == 0 {
            return None;
        }
        let before = (1u16 << slot) - 1;
        let hash = self.child_hashes[(self.state_mask & before).count_ones() as usize];
        let stored = (self.leaf_storage).stored_mask(self.state_mask, self.tree_mask);
        let commit = if stored & (1 << slot) != 0 {
            self.child_commits[(stored & before).count_ones() as usize]
        } else {
            0
        };
        Some(if self.tree_mask & (1 << slot) != 0 {
            RootRef::Branch(BranchRef { hash, commit })
        } else {
            let path =
                self.leaf_paths[(self.state_mask & !self.tree_mask & before).count_ones() as usize];
            RootRef::Leaf(LeafRef { path, hash, commit })
        })
    }

    /// Hash payload, without the domain byte or stored-only paths and commits.
    pub fn hash_payload(&self) -> Vec<u8> {
        let mut bytes = vec![self.prefix_len];
        bytes.extend_from_slice(&self.prefix);
        bytes.extend_from_slice(&self.state_mask.to_be_bytes());
        bytes.extend_from_slice(&self.tree_mask.to_be_bytes());
        for hash in &self.child_hashes {
            bytes.extend_from_slice(hash);
        }
        bytes
    }

    /// Hash with the branch domain byte assigned by the owning crate.
    pub fn hash(&self, domain: u8, hasher: &impl HashProvider) -> Hash {
        hasher.hash_parts(&[&[domain], &self.hash_payload()])
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = self.hash_payload();
        for path in &self.leaf_paths {
            bytes.extend_from_slice(path);
        }
        for commit in &self.child_commits {
            bytes.extend_from_slice(&commit.to_be_bytes());
        }
        bytes
    }

    /// Strict decoding: rejects trailing bytes, bad masks and nonzero padding.
    /// Depth and leaf routing are checked when the trie reads this branch.
    ///
    /// ```compile_fail,E0080
    /// use golemdb_merkle::BranchNodeCompact;
    /// use golemdb_merkle::LeafStorage;
    /// let _ = BranchNodeCompact::<0>::decode(&[], LeafStorage::InNode);
    /// ```
    pub fn decode(bytes: &[u8], leaf_storage: LeafStorage) -> Result<Self> {
        const {
            assert!(
                N >= 1 && N <= 32,
                "Merkle paths must contain between 1 and 32 bytes"
            )
        };
        let bad = || MerkleError::InvalidNode("invalid encoded length");
        let prefix_len = *bytes.first().ok_or_else(bad)?;
        let end = 1 + (prefix_len as usize).div_ceil(2);
        let header = bytes.get(..end + 4).ok_or_else(bad)?;
        let state_mask = u16::from_be_bytes([header[end], header[end + 1]]);
        let tree_mask = u16::from_be_bytes([header[end + 2], header[end + 3]]);
        let hash_end = end + 4 + state_mask.count_ones() as usize * 32;
        let paths_end = hash_end + (state_mask & !tree_mask).count_ones() as usize * N;
        let commits_end =
            paths_end + leaf_storage.stored_mask(state_mask, tree_mask).count_ones() as usize * 8;
        if bytes.len() != commits_end {
            return Err(bad());
        }
        let child_hashes = bytes[end + 4..hash_end]
            .chunks_exact(32)
            .map(|b| b.try_into().unwrap())
            .collect();
        let leaf_paths = bytes[hash_end..paths_end]
            .chunks_exact(N)
            .map(|b| b.try_into().unwrap())
            .collect();
        let child_commits = bytes[paths_end..]
            .chunks_exact(8)
            .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
            .collect();
        Self::new(
            header[1..end].to_vec(),
            prefix_len,
            state_mask,
            tree_mask,
            child_hashes,
            leaf_paths,
            child_commits,
            leaf_storage,
        )
    }

    /// The prefix as one nibble per byte.
    pub fn unpack_prefix(&self) -> Vec<u8> {
        (0..self.prefix_len as usize)
            .map(|i| path::nibble(&self.prefix, i))
            .collect()
    }

    pub(crate) fn children(&self) -> [RootRef<N>; 16] {
        std::array::from_fn(|slot| self.child(slot as u8).unwrap_or(RootRef::Empty))
    }

    pub(crate) fn from_children(
        prefix: &[u8],
        children: &[RootRef<N>; 16],
        leaf_storage: LeafStorage,
    ) -> Result<Self> {
        let mut state = 0;
        let mut tree = 0;
        let mut hashes = Vec::new();
        let mut paths = Vec::new();
        let mut commits = Vec::new();
        for (slot, child) in children.iter().enumerate() {
            match child {
                RootRef::Empty => continue,
                RootRef::Leaf(leaf) => {
                    hashes.push(leaf.hash);
                    paths.push(leaf.path);
                    if leaf_storage == LeafStorage::Rows {
                        commits.push(leaf.commit);
                    }
                }
                RootRef::Branch(branch) => {
                    tree |= 1 << slot;
                    hashes.push(branch.hash);
                    commits.push(branch.commit);
                }
            }
            state |= 1 << slot;
        }
        Self::new(
            path::pack(prefix),
            prefix.len() as u8,
            state,
            tree,
            hashes,
            paths,
            commits,
            leaf_storage,
        )
    }

    pub(crate) fn validate_at(&self, ancestors: &[u8]) -> Result<()> {
        let mut route = ancestors.to_vec();
        route.extend(self.unpack_prefix());
        if route.len() >= N * 2 {
            return Err(MerkleError::InvalidNode("branch exceeds path depth"));
        }
        if route.len() + 1 == N * 2 && self.tree_mask != 0 {
            return Err(MerkleError::InvalidNode("branch child beyond final nibble"));
        }
        for slot in 0..16 {
            if let Some(RootRef::Leaf(leaf)) = self.child(slot)
                && (route
                    .iter()
                    .enumerate()
                    .any(|(i, n)| path::nibble(&leaf.path, i) != *n)
                    || path::nibble(&leaf.path, route.len()) != slot)
            {
                return Err(MerkleError::InvalidNode(
                    "leaf path does not match branch route",
                ));
            }
        }
        Ok(())
    }
}
