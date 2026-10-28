use std::collections::BTreeMap;

use golemdb_cells::{CellKey, CellNameRef, CellReader, CellValue, reserved, system};
use golemdb_storage::ReadTransaction;

use crate::{ModelError, ModelMetadata, model::validate_weight_name};

pub(crate) type StoredWeights = BTreeMap<u64, BTreeMap<String, u64>>;

/// Encode a model and its weights as reserved cells, without identity/binding
/// cells. The caller chooses its version/activation, validates inputs and owns
/// publication, commitments and physical key/value limit checks. No storage I/O.
pub fn model_cells(
    metadata: &ModelMetadata,
    weights: &BTreeMap<String, u64>,
) -> Vec<(CellKey, CellValue)> {
    let mut model = Vec::with_capacity(24);
    model.extend_from_slice(&metadata.activation_commit.to_be_bytes());
    model.extend_from_slice(&metadata.golemdb_code_version.to_be_bytes());
    model.extend_from_slice(&metadata.host_code_version.to_be_bytes());
    let version = metadata.version.to_be_bytes();
    let mut cells = vec![(
        CellKey::new(system::METERING_MODEL.id, CellNameRef::raw(&version)),
        CellValue::from_bytes(&model),
    )];
    for (name, weight) in weights {
        let mut key = version.to_vec();
        key.extend_from_slice(name.as_bytes());
        cells.push((
            CellKey::new(system::MODEL_WEIGHT.id, CellNameRef::raw(&key)),
            CellValue::from_u64(*weight),
        ));
    }
    cells
}

pub(crate) fn read_models<R: ReadTransaction>(
    reader: &CellReader<'_, R>,
) -> Result<BTreeMap<u64, ModelMetadata>, ModelError> {
    let mut models = BTreeMap::new();
    for entry in reader.scan_record(system::METERING_MODEL.id)? {
        let (key, value) = entry?;
        if key.name() == reserved::KEY {
            continue;
        }
        let version = decode_u64(key.name().as_bytes())?;
        let bytes = value
            .as_bytes()
            .filter(|bytes| bytes.len() == 24)
            .ok_or(ModelError::CorruptState("model value must be 24 bytes"))?;
        if value.is_indexable() || version == 0 {
            return Err(ModelError::CorruptState(
                "invalid model version or indexed model",
            ));
        }
        models.insert(
            version,
            ModelMetadata {
                version,
                activation_commit: decode_u64(&bytes[..8])?,
                golemdb_code_version: decode_u64(&bytes[8..16])?,
                host_code_version: decode_u64(&bytes[16..])?,
            },
        );
    }
    Ok(models)
}

pub(crate) fn read_weights<R: ReadTransaction>(
    reader: &CellReader<'_, R>,
    models: &BTreeMap<u64, ModelMetadata>,
) -> Result<StoredWeights, ModelError> {
    let mut weights = StoredWeights::new();
    for entry in reader.scan_record(system::MODEL_WEIGHT.id)? {
        let (key, value) = entry?;
        if key.name() == reserved::KEY {
            continue;
        }
        let key = key.name();
        let bytes = key.as_bytes();
        if bytes.len() <= 8 {
            return Err(ModelError::CorruptState("weight key has no version/name"));
        }
        let version = decode_u64(&bytes[..8])?;
        if !models.contains_key(&version) {
            return Err(ModelError::CorruptState("weight has no model"));
        }
        let name = std::str::from_utf8(&bytes[8..])
            .map_err(|_| ModelError::CorruptState("weight name is not UTF-8"))?;
        validate_weight_name(name).map_err(|_| ModelError::CorruptState("invalid weight name"))?;
        let weight = value
            .as_u64()
            .filter(|_| !value.is_indexable())
            .ok_or(ModelError::CorruptState("weight must be a non-indexed u64"))?;
        weights
            .entry(version)
            .or_default()
            .insert(name.into(), weight);
    }
    Ok(weights)
}

fn decode_u64(bytes: &[u8]) -> Result<u64, ModelError> {
    let bytes = bytes
        .try_into()
        .map_err(|_| ModelError::CorruptState("expected eight-byte integer"))?;
    Ok(u64::from_be_bytes(bytes))
}
