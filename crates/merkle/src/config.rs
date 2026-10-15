use crate::HashAlgorithm;
use serde::{Deserialize, Serialize};
use std::{error::Error, fmt, path::Path};

/// Standalone hash configuration for low-level runners. The public API's
/// Genesis also includes cell limits and persists the selected algorithm
/// in protocol metadata when opening a database.
/// Match the identifier once before entering the processing loop; each runner
/// instantiation uses a compile-time-known provider. The configuration is not a
/// runtime-switching provider. Changing algorithms requires a fresh/rebuilt
/// database; the API opening layer persists and checks the identifier.
///
/// ```
/// use golemdb_merkle::{Blake3Hasher, Hash, HashAlgorithm, HashConfig, HashProvider, Keccak256Hasher};
///
/// // The future engine's processing loop belongs inside this generic function.
/// fn run<H: HashProvider>(hasher: H) -> Hash {
///     hasher.hash(b"example")
/// }
/// let config = HashConfig::from_yaml("hash_function: blake3")?;
/// let hash = match config.hash_function {
///     HashAlgorithm::Keccak256 => run(Keccak256Hasher),
///     HashAlgorithm::Blake3 => run(Blake3Hasher),
/// };
/// assert_eq!(hash, Blake3Hasher.hash(b"example"));
/// # Ok::<(), golemdb_merkle::ConfigError>(())
/// ```
///
/// Deserialization requires the field explicitly, rejecting typos and duplicates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HashConfig {
    pub hash_function: HashAlgorithm,
}

impl HashConfig {
    pub fn from_yaml(yaml: &str) -> Result<Self, ConfigError> {
        serde_saphyr::from_str(yaml).map_err(ConfigError::Yaml)
    }

    /// Load a caller-selected file. No implicit search path or environment override.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let yaml = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        Self::from_yaml(&yaml)
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Yaml(serde_saphyr::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cannot read hash configuration: {error}"),
            Self::Yaml(error) => write!(f, "invalid hash configuration: {error}"),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Yaml(e) => Some(e),
        }
    }
}
