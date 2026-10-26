use std::{collections::BTreeMap, sync::Arc};

use golemdb_cells::CellReader;
use golemdb_storage::ReadTransaction;

use crate::{
    HostMeteringSchema, ModelError, ModelMetadata, Schedule, WeightName,
    model::declared_names,
    persisted::{read_models, read_weights},
};

/// Decoded active model, shared across calls and ordinary head advances.
#[derive(Debug)]
pub struct PricingModel {
    pub metadata: ModelMetadata,
    pub schedule: Schedule,
}

/// Per-call pricing commit plus a reference to immutable model data. The caller
/// supplies a commit from its validated storage view, never an independent head read.
#[derive(Debug, Clone)]
pub struct PricingSnapshot {
    pub commit_id: u64,
    pub model: Arc<PricingModel>,
}

/// Initial pricing cache. Load once from a committed snapshot at opening/recovery;
/// share this provider between consumers. Snapshot/weight access performs no I/O.
///
/// Runtime admin writes are not supported yet. After they are introduced, the
/// publication path must replace this provider coherently with the committed head.
/// External low-level mutations require reopening. Rewind also requires reloading;
/// commit numbers alone are not a cache identity across rewinds.
#[derive(Debug, Clone)]
pub struct PricingProvider {
    loaded_at: u64,
    active: Arc<PricingModel>,
    pending: Option<ModelMetadata>,
    weights: BTreeMap<u64, Arc<BTreeMap<String, u64>>>,
}

impl PricingProvider {
    pub fn load<R: ReadTransaction>(
        reader: &CellReader<'_, R>,
        commit_id: u64,
        host_schemas: &[HostMeteringSchema],
    ) -> Result<Self, ModelError> {
        let models = read_models(reader)?;
        let active = models
            .values()
            .rev()
            .find(|model| model.activation_commit <= commit_id)
            .copied()
            .ok_or(ModelError::CorruptState("no active metering model"))?;
        let mut pending = models
            .values()
            .filter(|model| model.activation_commit > commit_id);
        let next = pending.next().copied();
        if pending.next().is_some() {
            return Err(ModelError::CorruptState("multiple pending metering models"));
        }
        let names = declared_names(
            active.golemdb_code_version,
            active.host_code_version,
            host_schemas,
        )?;
        let mut stored = read_weights(reader, &models)?;
        // A declared model with no weight cells is entirely unpriced, not missing.
        for version in models.keys() {
            stored.entry(*version).or_default();
        }
        let weights = &stored[&active.version];
        for name in names.iter().filter(|name| !weights.contains_key(**name)) {
            tracing::warn!(
                model_version = active.version,
                weight = *name,
                "missing metering weight"
            );
        }
        for name in weights.keys().filter(|name| !names.contains(name.as_str())) {
            tracing::warn!(
                model_version = active.version,
                weight = name.as_str(),
                "ignored undeclared metering weight"
            );
        }
        let schedule = Schedule::new(
            active.golemdb_code_version,
            weights
                .iter()
                .map(|(name, value)| (WeightName::from(name.clone()), *value)),
        )?;
        Ok(Self {
            loaded_at: commit_id,
            active: Arc::new(PricingModel {
                metadata: active,
                schedule,
            }),
            pending: next,
            weights: stored
                .into_iter()
                .map(|(version, weights)| (version, Arc::new(weights)))
                .collect(),
        })
    }

    /// Caller must have selected this commit in the same database instance's
    /// committed state. This is cache access, not a historical storage lookup.
    pub fn snapshot(&self, commit_id: u64) -> Result<PricingSnapshot, ModelError> {
        if commit_id < self.loaded_at
            || self
                .pending
                .is_some_and(|next| commit_id >= next.activation_commit)
        {
            return Err(ModelError::ReloadRequired(commit_id));
        }
        Ok(PricingSnapshot {
            commit_id,
            model: self.active.clone(),
        })
    }

    pub fn pending(&self) -> Option<ModelMetadata> {
        self.pending
    }

    /// None selects the active model. Maps are shared, not copied per lookup.
    pub fn model_weights(
        &self,
        version: Option<u64>,
    ) -> Result<Arc<BTreeMap<String, u64>>, ModelError> {
        let version = version.unwrap_or(self.active.metadata.version);
        self.weights
            .get(&version)
            .cloned()
            .ok_or(ModelError::ModelNotFound(version))
    }
}
