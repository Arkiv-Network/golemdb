use std::path::Path;

use golemdb_cells::CellLimits;
use golemdb_immutable_data::ImmutableDataConfig;
use golemdb_merkle::HashAlgorithm;
use serde::{Deserialize, Serialize};

use crate::{OpenError, OpenResult};

/// Immutable startup values written at commit zero and validated on reopening.
/// YAML fields are required except a column's type (defaults to bytes).
/// No admission limit, shard span, compression or hash algorithm is silently
/// selected by the opener. Unknown fields are rejected so unsupported settings
/// cannot be silently ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Genesis {
    pub hash_function: HashAlgorithm,
    pub cell_limits: CellLimits,
    pub immutable_data: ImmutableDataConfig,
}

impl Genesis {
    pub const fn new(
        hash_function: HashAlgorithm,
        cell_limits: CellLimits,
        immutable_data: ImmutableDataConfig,
    ) -> Self {
        Self {
            hash_function,
            cell_limits,
            immutable_data,
        }
    }

    pub fn from_yaml(yaml: &str) -> OpenResult<Self> {
        let genesis: Self = serde_saphyr::from_str(yaml).map_err(OpenError::Yaml)?;
        genesis.validate_immutable_data()?;
        Ok(genesis)
    }

    /// Read only this explicit path; no environment or implicit file search.
    pub fn load(path: impl AsRef<Path>) -> OpenResult<Self> {
        Self::from_yaml(&std::fs::read_to_string(path).map_err(OpenError::Io)?)
    }

    pub(crate) fn validate(&self, max_key: usize, max_value: usize) -> OpenResult<()> {
        self.validate_immutable_data()?;
        let limits = self.cell_limits;
        if limits.max_cell_name_len == 0 {
            return Err(OpenError::InvalidConfig(
                "max_cell_name_len must be positive".into(),
            ));
        }
        // Complete cell keys include recordID. Index keys include a separator
        // and tag; fixed-width attributes may be wider than the string limit.
        let name = u64::from(limits.max_cell_name_len);
        let key = (name + 8)
            .max(name + 2 + u64::from(limits.max_str_len).max(32))
            .max(40);
        // Format 1 reserves 16 KiB for fixed engine values: trie nodes, metadata,
        // root pairs, and canonical single-u16-domain Roaring containers.
        let value = (1 + u64::from(limits.max_bytes_len.max(limits.max_str_len))).max(16 * 1024);
        if key > max_key as u64 {
            return Err(OpenError::InvalidConfig(format!(
                "configured cells/index terms need {key}-byte keys; backend supports {max_key}"
            )));
        }
        if value > max_value as u64 {
            return Err(OpenError::InvalidConfig(format!(
                "configured cells and engine rows need {value}-byte values; backend supports {max_value}"
            )));
        }
        Ok(())
    }

    fn validate_immutable_data(&self) -> OpenResult<()> {
        self.immutable_data
            .validate()
            .map_err(|error| OpenError::InvalidConfig(error.to_string()))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OpenMode {
    /// Initialize storage with no tables or rows, otherwise validate and reopen.
    #[default]
    CreateIfMissing,
    /// Require a complete initialized database. Never write genesis.
    ExistingOnly,
    /// Require storage with no tables or rows. An existing empty directory is allowed.
    CreateNew,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpenConfig {
    pub genesis: Genesis,
    pub mode: OpenMode,
}

impl OpenConfig {
    pub const fn new(genesis: Genesis) -> Self {
        Self {
            genesis,
            mode: OpenMode::CreateIfMissing,
        }
    }

    pub const fn with_mode(mut self, mode: OpenMode) -> Self {
        self.mode = mode;
        self
    }
}
