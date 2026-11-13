/// Nibbles in a path byte, high nibble first.
pub(crate) const NIBBLES_PER_BYTE: usize = 2;

const NIBBLE_BITS: u32 = 4;

/// The low nibble of a byte.
pub(crate) const NIBBLE_MASK: u8 = 0x0f;

pub(crate) fn nibble(path: &[u8], depth: usize) -> u8 {
    let shift = if depth.is_multiple_of(NIBBLES_PER_BYTE) {
        NIBBLE_BITS
    } else {
        0
    };
    (path[depth / NIBBLES_PER_BYTE] >> shift) & NIBBLE_MASK
}

/// How many leading `nibbles` `path` repeats from nibble `depth` onwards.
pub(crate) fn matched(nibbles: &[u8], path: &[u8], depth: usize) -> usize {
    (nibbles.iter().enumerate())
        .take_while(|&(i, n)| nibble(path, depth + i) == *n)
        .count()
}

pub(crate) fn nibbles(path: &[u8]) -> Vec<u8> {
    (0..path.len() * NIBBLES_PER_BYTE)
        .map(|i| nibble(path, i))
        .collect()
}

pub(crate) fn pack(nibbles: &[u8]) -> Vec<u8> {
    nibbles
        .chunks(NIBBLES_PER_BYTE)
        .map(|pair| (pair[0] << NIBBLE_BITS) | pair.get(1).copied().unwrap_or(0))
        .collect()
}
