use std::sync::Arc;

use golemdb_cells::CellLimits;
use golemdb_metering::{INITIAL_DEPTHS, ModeledDepths, PricingProvider};

use crate::RecordLimits;

/// Immutable write configuration supplied by the owning connection layer.
/// Limits and provider must match the initialized deployment. Each write captures
/// the provider's snapshot at its validated branch origin before admission.
#[derive(Debug, Clone)]
pub struct WritePricing {
    pub provider: Arc<PricingProvider>,
    pub depths: ModeledDepths,
    pub cell_limits: CellLimits,
    pub record_limits: RecordLimits,
}

impl WritePricing {
    /// Share committed pricing with the owning connection and use the temporary
    /// modeled depths. Zero prices must be explicitly supplied in the model.
    pub fn new(
        provider: Arc<PricingProvider>,
        cell_limits: CellLimits,
        record_limits: RecordLimits,
    ) -> Self {
        Self {
            provider,
            depths: INITIAL_DEPTHS,
            cell_limits,
            record_limits,
        }
    }
}
