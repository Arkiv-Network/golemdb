use std::ops::BitOrAssign;

use roaring::RoaringTreemap;

use crate::{BitmapContainer, BitmapError};

/// A materialized query bitmap assembled from storage chunks.
///
/// Owns the chunk-to-record-ID conversion, not storage or Merkle hashing.
/// Set operations are operators on `Bitmap` (`|=` is union), so callers need
/// not name Roaring; [`Self::treemap`] reads the record IDs. The default is
/// the empty bitmap.
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

/// Union: `self` gains every record ID of `other`.
impl BitOrAssign for Bitmap {
    fn bitor_assign(&mut self, other: Self) {
        self.values |= other.values;
    }
}
