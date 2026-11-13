use crate::{Accounting, Ledger, PricingError, Schedule, WeightName as W};

/// Pricing inputs, not measured path lengths. Initial integration uses
/// [`INITIAL_DEPTHS`]; fixtures can supply other modeled depths explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeledDepths {
    pub cell_trie: u8,
    pub index_trie: u8,
}

/// Temporary policy until population counters and D3 depth selection are added.
pub const INITIAL_DEPTHS: ModeledDepths = ModeledDepths {
    cell_trie: 6,
    index_trie: 6,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOpKind {
    Create,
    Patch,
    Delete,
}

/// Completed planning work, in the reference procedure's execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanningStep {
    AdmissionBase,
    AdmissionEntry,
    AdmissionBytes(u64),
    /// One indivisible checkpoint: read.key + write.record.{op}.
    RecordBase(RecordOpKind),
    CellRead,
    TermRead,
}

/// Prospective writes. User and per-record system cells use the same prices.
/// Deleted bytes are effects only: deletion variants cannot charge byte writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationCharge {
    CellCreate {
        bytes_written: u64,
    },
    CellUpdate {
        bytes_written: u64,
    },
    CellDelete,
    IndexJoin {
        bytes_written: u64,
        create_term: bool,
    },
    IndexLeave,
}

impl PlanningStep {
    pub fn price(self, schedule: &Schedule) -> Result<Accounting, PricingError> {
        let counts = match self {
            Self::AdmissionBase => vec![(W::WRITE_ADMISSION_BASE, 1)],
            Self::AdmissionEntry => vec![(W::WRITE_ADMISSION_ENTRY, 1)],
            Self::AdmissionBytes(bytes) => vec![(W::WRITE_ADMISSION_BYTE, bytes)],
            Self::RecordBase(op) => vec![
                (W::READ_KEY, 1),
                (
                    match op {
                        RecordOpKind::Create => W::WRITE_RECORD_CREATE,
                        RecordOpKind::Patch => W::WRITE_RECORD_PATCH,
                        RecordOpKind::Delete => W::WRITE_RECORD_DELETE,
                    },
                    1,
                ),
            ],
            Self::CellRead => vec![(W::READ_CELL, 1)],
            Self::TermRead => vec![(W::READ_TERM, 1)],
        };
        schedule.account(Ledger::from_counts(counts))
    }
}

impl MutationCharge {
    pub fn price(
        self,
        schedule: &Schedule,
        depths: ModeledDepths,
    ) -> Result<Accounting, PricingError> {
        let counts = match self {
            Self::CellCreate { bytes_written } => vec![
                (W::WRITE_CELL_CREATE, 1),
                (W::WRITE_CELL_TRIE_UPDATE, depths.cell_trie.into()),
                (W::WRITE_CELL_BYTE, bytes_written),
            ],
            Self::CellUpdate { bytes_written } => vec![
                (W::WRITE_CELL_UPDATE, 1),
                (W::WRITE_CELL_TRIE_UPDATE, depths.cell_trie.into()),
                (W::WRITE_CELL_BYTE, bytes_written),
            ],
            Self::CellDelete => vec![
                (W::WRITE_CELL_DELETE, 1),
                (W::WRITE_CELL_TRIE_UPDATE, depths.cell_trie.into()),
            ],
            Self::IndexJoin {
                bytes_written,
                create_term,
            } => vec![
                (W::WRITE_INDEX_JOIN, 1),
                (W::WRITE_INDEX_TRIE_UPDATE, depths.index_trie.into()),
                (W::WRITE_INDEX_BYTE, bytes_written),
                (W::WRITE_INDEX_TERM_CREATE, u64::from(create_term)),
            ],
            Self::IndexLeave => vec![
                (W::WRITE_INDEX_LEAVE, 1),
                (W::WRITE_INDEX_TRIE_UPDATE, depths.index_trie.into()),
            ],
        };
        schedule.account(Ledger::from_counts(counts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WRITE_CODE_VERSION;

    #[test]
    fn mutation_prices_use_the_right_paths_bytes_and_optional_term_creation() {
        let schedule = Schedule::new(
            WRITE_CODE_VERSION,
            [
                (W::WRITE_CELL_CREATE, 5),
                (W::WRITE_CELL_UPDATE, 6),
                (W::WRITE_CELL_DELETE, 4),
                (W::WRITE_CELL_TRIE_UPDATE, 2),
                (W::WRITE_CELL_BYTE, 3),
                (W::WRITE_INDEX_JOIN, 7),
                (W::WRITE_INDEX_LEAVE, 8),
                (W::WRITE_INDEX_TRIE_UPDATE, 3),
                (W::WRITE_INDEX_BYTE, 2),
                (W::WRITE_INDEX_TERM_CREATE, 11),
            ],
        )
        .unwrap();
        let depths = ModeledDepths {
            cell_trie: 4,
            index_trie: 6,
        };
        for (mutation, expected) in [
            (MutationCharge::CellCreate { bytes_written: 10 }, 43),
            (MutationCharge::CellUpdate { bytes_written: 10 }, 44),
            (MutationCharge::CellDelete, 12),
            (
                MutationCharge::IndexJoin {
                    bytes_written: 10,
                    create_term: false,
                },
                45,
            ),
            (
                MutationCharge::IndexJoin {
                    bytes_written: 10,
                    create_term: true,
                },
                56,
            ),
            (MutationCharge::IndexLeave, 26),
        ] {
            let charge = mutation.price(&schedule, depths).unwrap();
            assert_eq!(charge.cost, expected, "{mutation:?}");
            assert_eq!(schedule.price(&charge.ledger).unwrap(), expected);
            // Reads are separate planning steps, never folded into mutations.
            assert!(
                charge
                    .ledger
                    .iter()
                    .all(|(name, _)| !name.as_str().starts_with("read."))
            );
        }
    }

    #[test]
    fn deletion_and_existing_term_joins_need_no_prepaid_or_deleted_byte_weight() {
        let schedule = Schedule::new(
            WRITE_CODE_VERSION,
            [
                (W::WRITE_CELL_DELETE, 4),
                (W::WRITE_CELL_TRIE_UPDATE, 1),
                (W::WRITE_INDEX_LEAVE, 2),
                (W::WRITE_INDEX_TRIE_UPDATE, 1),
                (W::WRITE_INDEX_JOIN, 3),
                (W::WRITE_INDEX_BYTE, 1),
            ],
        )
        .unwrap();
        assert_eq!(
            MutationCharge::CellDelete
                .price(&schedule, INITIAL_DEPTHS)
                .unwrap()
                .cost,
            10
        );
        assert_eq!(
            MutationCharge::IndexLeave
                .price(&schedule, INITIAL_DEPTHS)
                .unwrap()
                .cost,
            8
        );
        assert_eq!(
            MutationCharge::IndexJoin {
                bytes_written: 10,
                create_term: false
            }
            .price(&schedule, INITIAL_DEPTHS)
            .unwrap()
            .cost,
            19
        );
        assert_eq!(
            MutationCharge::IndexJoin {
                bytes_written: 10,
                create_term: true
            }
            .price(&schedule, INITIAL_DEPTHS)
            .unwrap_err(),
            PricingError::Unpriced(W::WRITE_INDEX_TERM_CREATE)
        );
    }

    #[test]
    fn lookup_charges_do_not_depend_on_whether_the_lookup_finds_data() {
        let schedule = Schedule::new(
            WRITE_CODE_VERSION,
            [
                (W::READ_KEY, 1),
                (W::READ_CELL, 2),
                (W::READ_TERM, 3),
                (W::WRITE_RECORD_CREATE, 4),
                (W::WRITE_RECORD_PATCH, 5),
                (W::WRITE_RECORD_DELETE, 6),
            ],
        )
        .unwrap();
        for (step, expected) in [
            (PlanningStep::RecordBase(RecordOpKind::Create), 5),
            (PlanningStep::RecordBase(RecordOpKind::Patch), 6),
            (PlanningStep::RecordBase(RecordOpKind::Delete), 7),
            (PlanningStep::CellRead, 2),
            (PlanningStep::TermRead, 3),
        ] {
            assert_eq!(step.price(&schedule).unwrap().cost, expected);
        }
        // No byte-validation work does not require a byte price.
        assert_eq!(
            PlanningStep::AdmissionBytes(0).price(&schedule).unwrap(),
            Accounting::default()
        );
    }
}
