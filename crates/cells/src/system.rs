//! Shared reserved-record catalogue. Values are schema constants, not admission policy.

/// Admin record IDs begin here; lower IDs are engine-managed system records.
pub const FIRST_ADMIN_RECORD_ID: u64 = 32;

/// User record IDs begin here; all lower IDs belong to the engine.
pub const FIRST_USER_RECORD_ID: u64 = 64;

/// Classify by the reserved ID range, not the record key's display prefix.
pub fn is_admin_record(record_id: u64) -> bool {
    (FIRST_ADMIN_RECORD_ID..FIRST_USER_RECORD_ID).contains(&record_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemRecord {
    pub id: u64,
    pub key: [u8; 32],
}

const fn record(id: u64, name: &[u8]) -> SystemRecord {
    let mut key = [0; 32];
    let mut i = 0;
    while i < name.len() {
        key[i] = name[i];
        i += 1;
    }
    SystemRecord { id, key }
}

pub const PARAMS: SystemRecord = record(0, b"#params");
pub const ALLOC: SystemRecord = record(1, b"#alloc");
pub const ROOTS: SystemRecord = record(2, b"#roots");
pub const RECORD_KEYS: SystemRecord = record(3, b"#recordKeys");
pub const ROOT_INDEX: SystemRecord = record(4, b"#rootIndex");
pub const METERING_MODEL: SystemRecord = record(32, b"@meteringModel");
pub const MODEL_WEIGHT: SystemRecord = record(33, b"@modelWeight");

/// Exact reserved keys, including admin records. No key prefix is reserved.
pub const ALL: &[SystemRecord] = &[
    PARAMS,
    ALLOC,
    ROOTS,
    RECORD_KEYS,
    ROOT_INDEX,
    METERING_MODEL,
    MODEL_WEIGHT,
];

pub fn by_key(key: &[u8; 32]) -> Option<SystemRecord> {
    ALL.iter().copied().find(|record| &record.key == key)
}
