//! Execution-only request admission. Called after handle/snapshot selection and
//! before any record lookup or mutation. Checks are guarded by the same meter as
//! state-dependent planning. No public validation shortcut.

use std::collections::{BTreeMap, btree_map::Entry};

use golemdb_cells::{CellKind, CellLimits, CellName, CellNameError, CellValue, system};
use golemdb_metering::{PlanningStep, WriteMeter};

use crate::{
    CellPatch, CreateEntry, PatchChange, PatchEntry, RecordCells, RecordError, RecordKey,
    RecordPatch, Result,
};

pub(crate) fn base(key: RecordKey, meter: &mut WriteMeter<'_>) -> Result<()> {
    meter.run_step(PlanningStep::AdmissionBase, || {
        if system::by_key(&key.0).is_some() {
            return Err(RecordError::Reserved);
        }
        Ok(())
    })?
}

pub(crate) fn create(
    limits: &CellLimits,
    entries: Vec<CreateEntry>,
    meter: &mut WriteMeter<'_>,
) -> Result<RecordCells> {
    let mut cells = RecordCells::new();
    for CreateEntry { name, value, kind } in entries {
        let step = write_step(kind);
        let count = entry(limits, step, &name, Some(&value), meter)?;
        meter.run_step(PlanningStep::AdmissionBytes(count), || {
            validate_name(limits, step, &name)?;
            let value = with_kind(step, &name, value, kind)?;
            insert(&mut cells, step, name, value)
        })??;
    }
    Ok(cells)
}

pub(crate) fn patch(
    limits: &CellLimits,
    entries: Vec<PatchEntry>,
    meter: &mut WriteMeter<'_>,
) -> Result<RecordPatch> {
    let mut changes = RecordPatch::new();
    for PatchEntry { name, change } in entries {
        let (step, value) = match &change {
            PatchChange::Write { value, kind } => (write_step(*kind), Some(value)),
            PatchChange::Set(value) => ("set", Some(value)),
            PatchChange::Remove => ("remove", None),
        };
        let count = entry(limits, step, &name, value, meter)?;
        meter.run_step(PlanningStep::AdmissionBytes(count), || {
            validate_name(limits, step, &name)?;
            let change = match change {
                PatchChange::Write { value, kind } => {
                    CellPatch::Write(with_kind(step, &name, value, kind)?)
                }
                PatchChange::Set(value) => CellPatch::Set(value),
                PatchChange::Remove => CellPatch::Remove,
            };
            insert(&mut changes, step, name, change)
        })??;
    }
    Ok(changes)
}

fn write_step(kind: CellKind) -> &'static str {
    match kind {
        CellKind::Field => "field",
        CellKind::Attribute => "attribute",
    }
}

fn entry(
    limits: &CellLimits,
    step: &str,
    name: &CellName,
    value: Option<&CellValue>,
    meter: &mut WriteMeter<'_>,
) -> Result<u64> {
    meter.run_step(PlanningStep::AdmissionEntry, || {
        validate_lengths(limits, step, name, value)?;
        // Names and payloads count; the fixed-size type/kind header belongs to
        // AdmissionEntry. Canonical typed value construction is host work.
        u64::try_from(name.as_bytes().len())
            .ok()
            .and_then(|name_bytes| {
                u64::try_from(value.map_or(0, |v| v.value().len()))
                    .ok()
                    .and_then(|value_bytes| name_bytes.checked_add(value_bytes))
            })
            .ok_or_else(|| RecordError::InvalidInput {
                message: "write input byte count overflow".into(),
                source: None,
            })
    })?
}

fn validate_lengths(
    limits: &CellLimits,
    step: &str,
    name: &CellName,
    value: Option<&CellValue>,
) -> Result<()> {
    // Inspect declared lengths before traversing names or preparing map entries.
    let max = limits.max_cell_name_len as usize;
    if name.as_bytes().len() > max {
        return Err(invalid(
            step,
            name,
            "invalid cell name",
            Some(Box::new(CellNameError::TooLong {
                max,
                actual: name.as_bytes().len(),
            })),
        ));
    }
    if let Some(value) = value {
        limits
            .validate_value(value.as_view())
            .map_err(|source| invalid(step, name, "invalid cell value", Some(Box::new(source))))?;
    }
    Ok(())
}

fn validate_name(limits: &CellLimits, step: &str, name: &CellName) -> Result<()> {
    // Name bytes are validated once, within execution. CellValue is already a
    // canonical typed value; construction/transport decoding is host work.
    limits
        .parse_user_name(name.as_bytes())
        .map_err(|source| invalid(step, name, "invalid cell name", Some(Box::new(source))))?;
    Ok(())
}

fn with_kind(step: &str, name: &CellName, value: CellValue, kind: CellKind) -> Result<CellValue> {
    value
        .with_kind(kind)
        .map_err(|source| invalid(step, name, "invalid cell value", Some(Box::new(source))))
}

fn insert<T>(
    values: &mut BTreeMap<CellName, T>,
    step: &str,
    name: CellName,
    value: T,
) -> Result<()> {
    match values.entry(name) {
        Entry::Vacant(entry) => {
            entry.insert(value);
            Ok(())
        }
        Entry::Occupied(entry) => Err(invalid(
            step,
            entry.key(),
            "the operation already writes or removes this cell",
            None,
        )),
    }
}

fn invalid(
    step: &str,
    name: &CellName,
    message: &str,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
) -> RecordError {
    // A length rejection must not scan/copy an arbitrarily large name just to
    // format its diagnostic. The typed source still carries the full length.
    let bytes = name.as_bytes();
    let shown = &bytes[..bytes.len().min(64)];
    let suffix = if shown.len() < bytes.len() { "..." } else { "" };
    RecordError::InvalidInput {
        message: format!("{step}(\"{}{suffix}\"): {message}", shown.escape_ascii()),
        source,
    }
}
