//! Read-only CUD planning. A plan never escapes the branch guard that produced it.
//! Branch supplies cells and opaque location metadata; this module decides the
//! mutations, computes their effects and prices them before application.

use golemdb_branch::{CellLocation, CellRead, CellWrite};
use golemdb_cells::{CellKey, CellName, CellValue, reserved};
use golemdb_metering::{MutationCharge, PlanningStep, RecordOpKind, WriteMeter};
use golemdb_storage::ReadTransaction;

use crate::{
    CellPatch, Effects, RecordCells, RecordError, RecordKey, RecordLimits, RecordMeta, RecordPatch,
    Result, cell_size, index_size, meta, state,
};

enum CellMutation {
    Put { key: CellKey, value: CellValue },
    Delete(PlannedDelete),
}

/// Record-owned decision to delete a cell after the whole write is approved.
/// User cells retain branch location metadata for scan maintenance; system cells
/// are outside user scans and need only their key. Old values are used while
/// computing effects and prices; branch journaling owns the undo information.
struct PlannedDelete {
    key: CellKey,
    location: Option<CellLocation>,
}

#[derive(Default)]
pub(crate) struct WritePlan {
    cells: Vec<CellMutation>,
    allocator: Option<CellValue>,
    pub effects: Effects,
}

impl WritePlan {
    fn cell(
        &mut self,
        key: CellKey,
        before: Option<CellValue>,
        after: Option<CellValue>,
        location: Option<CellLocation>,
        meter: &mut WriteMeter<'_>,
    ) -> Result<()> {
        if before == after {
            return Ok(());
        }
        let name: CellName = key.name().into();
        if key.record_id() >= golemdb_cells::system::FIRST_USER_RECORD_ID {
            self.effects
                .change(&name, before.as_ref(), after.as_ref())?;
        }
        let charge = match (before.as_ref(), after.as_ref()) {
            (None, Some(value)) => MutationCharge::CellCreate {
                bytes_written: cell_size(&name, value)?,
            },
            (Some(_), Some(value)) => MutationCharge::CellUpdate {
                bytes_written: cell_size(&name, value)?,
            },
            (Some(_), None) => MutationCharge::CellDelete,
            (None, None) => return Ok(()),
        };
        meter.add_planned_mutation(charge)?;
        self.cells.push(match after {
            Some(value) => CellMutation::Put { key, value },
            None => CellMutation::Delete(PlannedDelete { key, location }),
        });
        Ok(())
    }

    /// Application has no state reads, validation or pricing decisions. Branch
    /// journaling retains inverses; sealing still owns the final index changes.
    pub(crate) fn apply<R: ReadTransaction>(self, writer: &mut CellWrite<'_, R>) -> Effects {
        for mutation in self.cells {
            match mutation {
                CellMutation::Put { key, value } => writer.put(key, value),
                CellMutation::Delete(PlannedDelete { key, location }) => match location {
                    Some(location) => writer.delete_at(location),
                    None => writer.delete(key),
                },
            }
        }
        if let Some(next) = self.allocator {
            writer.put(state::allocator_key(), next);
        }
        self.effects
    }
}

fn binding<R: ReadTransaction>(
    reader: &CellRead<'_, R>,
    key: RecordKey,
    op: RecordOpKind,
    meter: &mut WriteMeter<'_>,
) -> Result<Option<u64>> {
    meter.run_step(PlanningStep::RecordBase(op), || {
        state::record_id(key, reader.get(&state::binding(key))?)
    })?
}

fn read<R: ReadTransaction>(
    reader: &CellRead<'_, R>,
    key: &CellKey,
    meter: &mut WriteMeter<'_>,
) -> Result<Option<CellValue>> {
    Ok(meter.run_step(PlanningStep::CellRead, || reader.get(key))??)
}

fn identity<R: ReadTransaction>(
    reader: &CellRead<'_, R>,
    id: u64,
    key: RecordKey,
    meter: &mut WriteMeter<'_>,
) -> Result<CellValue> {
    let value = read(reader, &CellKey::new(id, reserved::KEY), meter)?;
    state::check_identity(key, value.clone())?;
    value.ok_or(RecordError::CorruptState("bound record has no #key"))
}

fn indexes<R: ReadTransaction>(
    reader: &CellRead<'_, R>,
    name: &CellName,
    before: Option<&CellValue>,
    after: Option<&CellValue>,
    meter: &mut WriteMeter<'_>,
) -> Result<()> {
    if before == after {
        return Ok(());
    }
    if let Some(value) = before.filter(|v| v.is_indexable()) {
        meter.run_step(PlanningStep::TermRead, || {
            reader.origin_term_exists(name.as_view(), value)
        })??;
        meter.add_planned_mutation(MutationCharge::IndexLeave)?;
    }
    if let Some(value) = after.filter(|v| v.is_indexable()) {
        let exists = meter.run_step(PlanningStep::TermRead, || {
            reader.origin_term_exists(name.as_view(), value)
        })??;
        meter.add_planned_mutation(MutationCharge::IndexJoin {
            bytes_written: index_size(name, value)?,
            create_term: !exists,
        })?;
    }
    Ok(())
}

pub(crate) fn create<R: ReadTransaction>(
    reader: &CellRead<'_, R>,
    key: RecordKey,
    values: RecordCells,
    caps: RecordLimits,
    meter: &mut WriteMeter<'_>,
) -> Result<WritePlan> {
    if binding(reader, key, RecordOpKind::Create, meter)?.is_some() {
        return Err(RecordError::AlreadyExists);
    }
    // Fixed allocator preparation is covered by write.record.create. Merely
    // propose the next ID; only successful application advances it.
    let (id, next) = state::next_id(reader)?;
    if reader
        .scan_prefix(&id.to_be_bytes())?
        .next()
        .transpose()?
        .is_some()
    {
        return Err(RecordError::CorruptState(
            "allocator points to occupied record",
        ));
    }
    let mut plan = WritePlan::default();
    for (name, value) in values {
        indexes(reader, &name, None, Some(&value), meter)?;
        plan.cell(CellKey::new(id, name), None, Some(value), None, meter)?;
    }
    let meta = RecordMeta::default().apply(&plan.effects)?;
    caps.check(meta)?;
    plan.cell(
        CellKey::new(id, reserved::META),
        None,
        Some(meta.encode()),
        None,
        meter,
    )?;
    plan.cell(
        CellKey::new(id, reserved::KEY),
        None,
        Some(CellValue::from_bytes32(key.0)),
        None,
        meter,
    )?;
    plan.cell(
        state::binding(key),
        None,
        Some(CellValue::from_u64(id)),
        None,
        meter,
    )?;
    plan.allocator = Some(CellValue::from_u64(next));
    Ok(plan)
}

pub(crate) fn patch<R: ReadTransaction>(
    reader: &CellRead<'_, R>,
    key: RecordKey,
    changes: RecordPatch,
    caps: RecordLimits,
    meter: &mut WriteMeter<'_>,
) -> Result<WritePlan> {
    let id = binding(reader, key, RecordOpKind::Patch, meter)?.ok_or(RecordError::NotFound)?;
    let meta_key = CellKey::new(id, reserved::META);
    let before_meta_value = read(reader, &meta_key, meter)?;
    let before_meta = meta::required(before_meta_value.as_ref())?;
    // Preserve the existing binding/identity invariant; this is a real cell
    // read and is charged explicitly rather than hidden in key resolution.
    identity(reader, id, key, meter)?;
    let mut plan = WritePlan::default();
    for (name, change) in changes {
        let address = CellKey::new(id, name.clone());
        let (before, location) = meter.run_step(PlanningStep::CellRead, || -> Result<_> {
            let before = reader.get(&address)?;
            // Location discovery is part of this charged read. It changes no
            // branch state; the record plan below owns the deletion decision.
            let location = if before.is_some() && matches!(change, CellPatch::Remove) {
                Some(reader.locate_cell(&address)?)
            } else {
                None
            };
            Ok((before, location))
        })??;
        let after =
            match change {
                CellPatch::Write(value) => Some(value),
                CellPatch::Set(value) => {
                    let old = before
                        .as_ref()
                        .ok_or_else(|| RecordError::CellNotFound { name: name.clone() })?;
                    Some(value.with_kind(old.kind()).map_err(|source| {
                        RecordError::InvalidInput {
                            message: format!("set({name:?}): invalid cell value for stored kind"),
                            source: Some(Box::new(source)),
                        }
                    })?)
                }
                CellPatch::Remove => None,
            };
        indexes(reader, &name, before.as_ref(), after.as_ref(), meter)?;
        plan.cell(address, before, after, location, meter)?;
    }
    let after_meta = before_meta.apply(&plan.effects)?;
    caps.check(after_meta)?;
    if after_meta != before_meta {
        plan.cell(
            meta_key,
            before_meta_value,
            Some(after_meta.encode()),
            None,
            meter,
        )?;
    }
    Ok(plan)
}

pub(crate) fn delete<R: ReadTransaction>(
    reader: &CellRead<'_, R>,
    key: RecordKey,
    meter: &mut WriteMeter<'_>,
) -> Result<WritePlan> {
    let id = binding(reader, key, RecordOpKind::Delete, meter)?.ok_or(RecordError::NotFound)?;
    let meta_key = CellKey::new(id, reserved::META);
    let meta_value = read(reader, &meta_key, meter)?;
    let meta = meta::required(meta_value.as_ref())?;
    let identity_value = identity(reader, id, key, meter)?;
    let mut plan = WritePlan::default();
    let mut cells = reader.scan_user_cells(id)?;
    for _ in 0..meta.cells {
        let (address, value, location) =
            meter.run_step(PlanningStep::CellRead, || -> Result<_> {
                let (address, value) = cells
                    .next()
                    .transpose()?
                    .ok_or(RecordError::CorruptState("#meta exceeds live cell count"))?;
                // Keep the origin-position read within this checkpoint. The
                // scanner and locator only read; application maintains indexes.
                let location = reader.locate_cell(&address)?;
                Ok((address, value, location))
            })??;
        indexes(reader, &address.name().into(), Some(&value), None, meter)?;
        plan.cell(address, Some(value), None, Some(location), meter)?;
    }
    // One bounded end probe is part of write.record.delete's fixed scan work.
    // Do not silently leave extra live cells behind when metadata is corrupt.
    if cells.next().transpose()?.is_some() || meta.apply(&plan.effects)? != RecordMeta::default() {
        return Err(RecordError::CorruptState(
            "#meta does not match deleted cells",
        ));
    }
    plan.cell(meta_key, meta_value, None, None, meter)?;
    plan.cell(
        CellKey::new(id, reserved::KEY),
        Some(identity_value),
        None,
        None,
        meter,
    )?;
    plan.cell(
        state::binding(key),
        Some(CellValue::from_u64(id)),
        None,
        None,
        meter,
    )?;
    Ok(plan)
}
