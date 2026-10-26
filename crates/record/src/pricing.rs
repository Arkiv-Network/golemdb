use golemdb_cells::CellLimits;
use golemdb_metering::{INITIAL_DEPTHS, ModeledDepths, Schedule};

use crate::RecordLimits;

/// Immutable write configuration supplied by the owning connection layer.
/// Limits must match the initialized deployment. The metering crate now loads
/// persisted schedules; connecting its provider to CUD remains iteration 6.
#[derive(Debug, Clone)]
pub struct WritePricing {
    pub schedule: Schedule,
    pub depths: ModeledDepths,
    pub cell_limits: CellLimits,
    pub record_limits: RecordLimits,
}

impl WritePricing {
    /// Free write pricing with full accounting and temporary modeled depths.
    /// Cell and record limits are explicit deployment values, never defaults.
    pub fn zero(cell_limits: CellLimits, record_limits: RecordLimits) -> Self {
        Self {
            schedule: Schedule::zero_write(),
            depths: INITIAL_DEPTHS,
            cell_limits,
            record_limits,
        }
    }
}
