use roaring::RoaringTreemap;

use crate::{BitmapContainer, BitmapError};

/// A materialized query bitmap assembled from storage chunks.
///
/// Owns the chunk-to-record-ID conversion, not storage or Merkle hashing. Use
/// [`Self::treemap`] for Roaring's set operations; [`Self::into_treemap`] hands
/// ownership to the query evaluator without a second copy. The default is the
/// empty bitmap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bitmap {
    values: RoaringTreemap,
}

impl Bitmap {
    /// Assemble chunks in strictly increasing path order, as produced by trie
    /// traversal. Empty working chunks are omitted, but their paths must still
    /// be unique and ordered. No sorting or duplicate overwrites are hidden here.
    pub fn from_containers(
        containers: impl IntoIterator<Item = BitmapContainer>,
    ) -> Result<Self, BitmapError> {
        let mut values = RoaringTreemap::new();
        let mut previous = None;
        for container in containers {
            let path = container.path();
            if let Some(prev) = previous {
                if path == prev {
                    return Err(BitmapError::DuplicatePath { path });
                }
                if path < prev {
                    return Err(BitmapError::UnsortedPaths {
                        previous: prev,
                        path,
                    });
                }
            }
            previous = Some(path);
            values
                .append(container.iter())
                .expect("validated paths and offsets are strictly ordered");
        }
        Ok(Self { values })
    }

    pub fn treemap(&self) -> &RoaringTreemap {
        &self.values
    }

    pub fn into_treemap(self) -> RoaringTreemap {
        self.values
    }
}
