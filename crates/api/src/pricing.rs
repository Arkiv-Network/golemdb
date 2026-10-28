use std::{collections::BTreeMap, sync::Arc};

use golemdb_metering::ModelMetadata;

use crate::CommitId;

/// Read-only model inspection. `commit_id` selects configuration state, not the
/// data commit of a historical record/query read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeteringModelInfo {
    pub commit_id: CommitId,
    pub active: MeteringModelMetadata,
    pub pending: Option<MeteringModelMetadata>,
}

/// Metadata shared by active and pending models. API uses a typed CommitId;
/// metering keeps numeric commits to remain independent of the branch crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeteringModelMetadata {
    pub version: u64,
    pub activation_commit: CommitId,
    pub golemdb_code_version: u64,
    pub host_code_version: u64,
}

impl From<ModelMetadata> for MeteringModelMetadata {
    fn from(model: ModelMetadata) -> Self {
        Self {
            version: model.version,
            activation_commit: CommitId::new(model.activation_commit),
            golemdb_code_version: model.golemdb_code_version,
            host_code_version: model.host_code_version,
        }
    }
}

/// Stored weights at a resolved committed head, including host values and
/// undeclared names ignored by the active schedule. Missing names stay absent.
/// The immutable map is shared across reads/clones, not copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelWeights {
    pub commit_id: CommitId,
    pub version: u64,
    pub weights: Arc<BTreeMap<String, u64>>,
}
