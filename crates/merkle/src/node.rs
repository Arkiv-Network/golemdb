use crate::{
    BranchRef, COMMIT_BYTES, HASH_BYTES, Hash, HashProvider, LeafRef, MerkleError, Result, RootRef,
    path::{self, NIBBLE_MASK, NIBBLES_PER_BYTE},
};

/// Children of a branch: one slot per nibble value.
pub const BRANCH_SLOTS: usize = 16;

/// Fewer children would make the branch a leaf or empty.
const MIN_BRANCH_CHILDREN: u32 = 2;

/// The widest routing path a trie supports: a route's nibble count must fit
/// the one-byte length of a row key.
pub const MAX_PATH_BYTES: usize = 127;

/// Bytes of the leading prefix length in an encoded branch.
const PREFIX_LEN_BYTES: usize = 1;

/// Bytes of each of a branch's two child masks, one bit per slot.
const MASK_BYTES: usize = size_of::<u16>();
const _: () = assert!(u16::BITS as usize == BRANCH_SLOTS);

/// Canonical compact branch. Leaf paths and child commits are stored routing
/// metadata and are excluded from the hash; the owner must bind each full path
/// in its leaf hash. Child commits locate each branch child's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchNodeCompact<const N: usize> {
    prefix: Vec<u8>,
    prefix_len: u8,
    state_mask: u16,
    tree_mask: u16,
    child_hashes: Vec<Hash>,
    leaf_paths: Vec<[u8; N]>,
    child_commits: Vec<u64>,
}

impl<const N: usize> BranchNodeCompact<N> {
    /// `prefix` is packed high nibble first, relative to this branch's depth.
    /// Hashes, leaf paths and commits follow ascending occupied slots in their
    /// masks; there is one commit per branch child. Path widths outside
    /// 1..=[`MAX_PATH_BYTES`] fail at compile time; malformed branch contents
    /// still return runtime errors.
    ///
    /// ```compile_fail,E0080
    /// use golemdb_merkle::{BranchNodeCompact, MAX_PATH_BYTES};
    /// type TooWide = BranchNodeCompact<{ MAX_PATH_BYTES + 1 }>;
    /// let _ = TooWide::new(vec![], 0, 3, 3, vec![[0; 32]; 2], vec![], vec![0; 2]);
    /// ```
    pub fn new(
        prefix: Vec<u8>,
        prefix_len: u8,
        state_mask: u16,
        tree_mask: u16,
        child_hashes: Vec<Hash>,
        leaf_paths: Vec<[u8; N]>,
        child_commits: Vec<u64>,
    ) -> Result<Self> {
        const { check_path_width::<N>() };
        if prefix_len as usize >= NIBBLES_PER_BYTE * N
            || prefix.len() != (prefix_len as usize).div_ceil(NIBBLES_PER_BYTE)
        {
            return Err(MerkleError::InvalidNode("invalid prefix length"));
        }
        if !(prefix_len as usize).is_multiple_of(NIBBLES_PER_BYTE)
            && prefix.last().is_some_and(|byte| byte & NIBBLE_MASK != 0)
        {
            return Err(MerkleError::InvalidNode("nonzero prefix padding"));
        }
        if state_mask.count_ones() < MIN_BRANCH_CHILDREN
            || tree_mask & !state_mask != 0
            || child_hashes.len() != state_mask.count_ones() as usize
            || leaf_paths.len() != (state_mask & !tree_mask).count_ones() as usize
            || child_commits.len() != tree_mask.count_ones() as usize
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
        if usize::from(slot) >= BRANCH_SLOTS || self.state_mask & (1 << slot) == 0 {
            return None;
        }
        let before = (1u16 << slot) - 1;
        let hash = self.child_hashes[(self.state_mask & before).count_ones() as usize];
        Some(if self.tree_mask & (1 << slot) != 0 {
            let commit = self.child_commits[(self.tree_mask & before).count_ones() as usize];
            RootRef::Branch(BranchRef { hash, commit })
        } else {
            let path =
                self.leaf_paths[(self.state_mask & !self.tree_mask & before).count_ones() as usize];
            RootRef::Leaf(LeafRef::new(path, hash))
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
    /// let _ = BranchNodeCompact::<0>::decode(&[]);
    /// ```
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const { check_path_width::<N>() };
        let bad = || MerkleError::InvalidNode("invalid encoded length");
        let prefix_len = *bytes.first().ok_or_else(bad)?;
        let end = PREFIX_LEN_BYTES + (prefix_len as usize).div_ceil(NIBBLES_PER_BYTE);
        let masks = bytes.get(end..).ok_or_else(bad)?;
        let (state_mask, masks) = masks.split_first_chunk::<MASK_BYTES>().ok_or_else(bad)?;
        let (tree_mask, children) = masks.split_first_chunk::<MASK_BYTES>().ok_or_else(bad)?;
        let (state_mask, tree_mask) = (
            u16::from_be_bytes(*state_mask),
            u16::from_be_bytes(*tree_mask),
        );
        // Offsets within `children`: hashes, then leaf paths, then commits.
        let hash_end = state_mask.count_ones() as usize * HASH_BYTES;
        let paths_end = hash_end + (state_mask & !tree_mask).count_ones() as usize * N;
        let commits_end = paths_end + tree_mask.count_ones() as usize * COMMIT_BYTES;
        if children.len() != commits_end {
            return Err(bad());
        }
        // Each section is a whole number of chunks, so no remainder is dropped.
        let child_hashes = children[..hash_end].as_chunks::<HASH_BYTES>().0.to_vec();
        let leaf_paths = children[hash_end..paths_end].as_chunks::<N>().0.to_vec();
        let child_commits = (children[paths_end..].as_chunks::<COMMIT_BYTES>().0.iter())
            .map(|commit| u64::from_be_bytes(*commit))
            .collect();
        Self::new(
            bytes[PREFIX_LEN_BYTES..end].to_vec(),
            prefix_len,
            state_mask,
            tree_mask,
            child_hashes,
            leaf_paths,
            child_commits,
        )
    }

    /// The prefix as one nibble per byte.
    pub fn unpack_prefix(&self) -> Vec<u8> {
        (0..self.prefix_len as usize)
            .map(|i| path::nibble(&self.prefix, i))
            .collect()
    }

    pub(crate) fn children(&self) -> [RootRef<N>; BRANCH_SLOTS] {
        std::array::from_fn(|slot| self.child(slot as u8).unwrap_or(RootRef::Empty))
    }

    pub(crate) fn from_children(
        prefix: &[u8],
        children: &[RootRef<N>; BRANCH_SLOTS],
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
        )
    }

    pub(crate) fn validate_at(&self, ancestors: &[u8]) -> Result<()> {
        let mut route = ancestors.to_vec();
        route.extend(self.unpack_prefix());
        if route.len() >= N * NIBBLES_PER_BYTE {
            return Err(MerkleError::InvalidNode("branch exceeds path depth"));
        }
        if route.len() + 1 == N * NIBBLES_PER_BYTE && self.tree_mask != 0 {
            return Err(MerkleError::InvalidNode("branch child beyond final nibble"));
        }
        for slot in 0..BRANCH_SLOTS as u8 {
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

/// Path widths a trie supports, checked at compile time.
pub(crate) const fn check_path_width<const N: usize>() {
    assert!(
        N >= 1 && N <= MAX_PATH_BYTES,
        "Merkle paths must contain between 1 and MAX_PATH_BYTES bytes"
    );
}
