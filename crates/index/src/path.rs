use crate::{BitmapError, CHUNK_BITS, HI48_BYTES, MAX_PATH, PATH_BYTE_OFFSET};

pub(crate) const fn is_valid_path(path: u64) -> bool {
    path <= MAX_PATH
}

pub const fn split(value: u64) -> (u64, u16) {
    (value >> CHUNK_BITS, value as u16)
}

pub const fn join(path: u64, value: u16) -> Result<u64, BitmapError> {
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
