use std::collections::BTreeMap;

use crate::{PricingError, WeightName};

/// Charged units per weight, in name order. Units can be lookups, entries, bytes
/// or modeled trie nodes. Explicitly free work still has a positive count.
/// This is separate from the effects a write applies to user data.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Ledger(BTreeMap<WeightName, u64>);

impl Ledger {
    /// Zero means the weight was not charged, not that it was unpriced.
    pub fn count(&self, name: &str) -> u64 {
        self.0.get(name).copied().unwrap_or(0)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&WeightName, u64)> {
        self.0.iter().map(|(name, count)| (name, *count))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn from_counts(counts: impl IntoIterator<Item = (WeightName, u64)>) -> Self {
        // Logical charge descriptions have at most one entry per weight.
        Self(
            counts
                .into_iter()
                .filter(|(_, count)| *count != 0)
                .collect(),
        )
    }

    pub(crate) fn checked_merge(&self, other: &Self) -> Result<Self, PricingError> {
        let mut merged = self.clone();
        for (name, count) in other.iter() {
            let value = merged.0.entry(name.clone()).or_default();
            *value = value.checked_add(count).ok_or(PricingError::Overflow)?;
        }
        Ok(merged)
    }
}

/// A cost and the charged units that explain it under one schedule.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Accounting {
    pub cost: u64,
    pub ledger: Ledger,
}

impl Accounting {
    pub(crate) fn checked_merge(&self, other: &Self) -> Result<Self, PricingError> {
        Ok(Self {
            cost: self
                .cost
                .checked_add(other.cost)
                .ok_or(PricingError::Overflow)?,
            ledger: self.ledger.checked_merge(&other.ledger)?,
        })
    }
}
