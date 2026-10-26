use crate::{BitmapError, CHUNK_BITS, HI48_BYTES, MAX_OFFSET, MAX_PATH, PATH_BYTE_OFFSET};

pub(crate) const fn is_valid_path(path: u64) -> bool {
    path <= MAX_PATH
}

pub const fn split(value: u64) -> (u64, u16) {
    (value >> CHUNK_BITS, (value & MAX_OFFSET as u64) as u16)
}

pub const fn join(path: u64, value: u16) -> Result<u64, BitmapError> {
    if value as u32 > MAX_OFFSET {
        return Err(BitmapError::OffsetOutOfRange {
            value: value as u32,
        });
    }
    if !is_valid_path(path) {
        return Err(BitmapError::PathOutOfRange { path });
    }
    Ok(path << CHUNK_BITS | value as u64)
}

pub const fn path_bytes(path: u64) -> Result<[u8; HI48_BYTES], BitmapError> {
    if !is_valid_path(path) {
        return Err(BitmapError::PathOutOfRange { path });
    }
    let be = path.to_be_bytes();
    let mut out = [0u8; HI48_BYTES];
    let mut i = 0;
    // A plain loop rather than `copy_from_slice`, which is not const.
    while i < HI48_BYTES {
        out[i] = be[PATH_BYTE_OFFSET + i];
        i += 1;
    }
    Ok(out)
}

/// Reverse [`path_bytes`].
pub(crate) fn from_bytes(bytes: &[u8; HI48_BYTES]) -> u64 {
    let mut be = [0u8; 8];
    be[PATH_BYTE_OFFSET..].copy_from_slice(bytes);
    u64::from_be_bytes(be)
}
