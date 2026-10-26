use std::collections::{BTreeMap, BTreeSet};

use golemdb_cells::CellNameRef;
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::WRITE_CODE_VERSION;

/// Names declared by GolemDB code version 1 (metering D10). Declaring read/log
/// names permits their configuration; their execution metering is still deferred.
pub const GOLEMDB_WEIGHT_NAMES: &[&str] = &[
    "write.admission.base",
    "write.admission.entry",
    "write.admission.byte",
    "write.record.create",
    "write.record.patch",
    "write.record.delete",
    "write.cell.create",
    "write.cell.update",
    "write.cell.delete",
    "write.cell_trie_update",
    "write.cell_byte",
    "write.index.join",
    "write.index.leave",
    "write.index_term_create",
    "write.index_trie_update",
    "write.index_byte",
    "read.key",
    "read.cell",
    "read.cell_scan",
    "read.byte",
    "read.term",
    "read.base",
    "read.admission.base",
    "read.admission.entry",
    "read.admission.byte",
    "read.term_step",
    "read.container",
    "read.set_container",
    "read.sort_compare",
    "write.log_append.base",
    "write.log_append.byte",
    "read.log_row",
];

/// Weight names understood by one host metering code version. Supplied at
/// opening as local capabilities; actual weight values belong to PricingConfig.
/// Code version zero is reserved for a host that declares no weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMeteringSchema {
    pub code_version: u64,
    pub weight_names: Vec<String>,
}

/// Code versions and weight values, including host weights. The caller supplies
/// the model version and activation commit separately (genesis uses 1 and 0).
/// YAML parsing is owned by API; callers can also construct this directly in Rust.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PricingConfig {
    pub golemdb_code_version: u64,
    pub host_code_version: u64,
    #[serde(deserialize_with = "unique_weights")]
    pub weights: BTreeMap<String, u64>,
}

impl PricingConfig {
    /// Explicit free prices, useful for tests/development. Missing configuration
    /// never invokes this constructor implicitly.
    pub fn zero() -> Self {
        Self {
            golemdb_code_version: WRITE_CODE_VERSION,
            host_code_version: 0,
            weights: GOLEMDB_WEIGHT_NAMES
                .iter()
                .map(|name| ((*name).into(), 0))
                .collect(),
        }
    }

    pub fn validate(&self, host_schemas: &[HostMeteringSchema]) -> Result<(), ModelError> {
        let declared = declared_names(
            self.golemdb_code_version,
            self.host_code_version,
            host_schemas,
        )?;
        for name in self.weights.keys() {
            validate_weight_name(name)?;
            if !declared.contains(name.as_str()) {
                return Err(ModelError::InvalidConfig(format!(
                    "undeclared weight {name}"
                )));
            }
        }
        Ok(())
    }
}

/// Installed model identity and activation. Several models may use the same
/// GolemDB/host code versions with different weight values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelMetadata {
    pub version: u64,
    pub activation_commit: u64,
    pub golemdb_code_version: u64,
    pub host_code_version: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("invalid metering configuration: {0}")]
    InvalidConfig(String),
    #[error("malformed persisted pricing: {0}")]
    CorruptState(&'static str),
    #[error("unsupported GolemDB metering code version {0}")]
    UnsupportedGolemDbCode(u64),
    #[error("unsupported host metering code version {0}")]
    UnsupportedHostCode(u64),
    #[error("metering model {0} not found")]
    ModelNotFound(u64),
    #[error("pricing cache must be reloaded for commit {0}")]
    ReloadRequired(u64),
    #[error(transparent)]
    Cells(#[from] golemdb_cells::CellError),
    #[error(transparent)]
    Schedule(#[from] crate::ScheduleError),
}

pub(crate) fn validate_weight_name(name: &str) -> Result<(), ModelError> {
    if !["write.", "read.", "host."]
        .iter()
        .any(|prefix| name.starts_with(prefix) && name.len() > prefix.len())
        || CellNameRef::parse_user(name.as_bytes(), usize::MAX).is_err()
    {
        return Err(ModelError::InvalidConfig(format!(
            "invalid weight name {name:?}"
        )));
    }
    Ok(())
}

pub(crate) fn declared_names(
    golemdb_code_version: u64,
    host_version: u64,
    host_schemas: &[HostMeteringSchema],
) -> Result<BTreeSet<&str>, ModelError> {
    let mut versions = BTreeSet::new();
    for host in host_schemas {
        if host.code_version == 0 || !versions.insert(host.code_version) {
            return Err(ModelError::InvalidConfig(
                "host code versions must be nonzero and unique".into(),
            ));
        }
        let mut names = BTreeSet::new();
        for name in &host.weight_names {
            validate_weight_name(name)?;
            if !name.starts_with("host.") || !names.insert(name) {
                return Err(ModelError::InvalidConfig(format!(
                    "invalid or duplicate host weight {name}"
                )));
            }
        }
    }
    if golemdb_code_version != WRITE_CODE_VERSION {
        return Err(ModelError::UnsupportedGolemDbCode(golemdb_code_version));
    }
    let mut names: BTreeSet<&str> = GOLEMDB_WEIGHT_NAMES.iter().copied().collect();
    if host_version != 0 {
        let host = host_schemas
            .iter()
            .find(|host| host.code_version == host_version)
            .ok_or(ModelError::UnsupportedHostCode(host_version))?;
        names.extend(host.weight_names.iter().map(String::as_str));
    }
    Ok(names)
}

// Maps normally overwrite duplicates during deserialization. Reject them before
// constructing the canonical map so YAML cannot silently choose a price.
fn unique_weights<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, u64>, D::Error> {
    struct Visitor;
    impl<'de> de::Visitor<'de> for Visitor {
        type Value = BTreeMap<String, u64>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map of unique weight names to u64 values")
        }
        fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut weights = BTreeMap::new();
            while let Some((name, value)) = map.next_entry::<String, u64>()? {
                if weights.insert(name.clone(), value).is_some() {
                    return Err(de::Error::custom(format!("duplicate weight {name}")));
                }
            }
            Ok(weights)
        }
    }
    deserializer.deserialize_map(Visitor)
}
