use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

pub type Hash = [u8; 32];

/// Bytes of a [`Hash`](type@Hash).
pub const HASH_BYTES: usize = size_of::<Hash>();

/// Hash the concatenation of byte slices, with no implicit framing or domain.
/// Callers define canonical preimages and supply domain bytes explicitly.
/// Use the same provider for routing, leaf hashes, and branch hashes.
/// Concrete providers keep algorithm selection out of the hashing path.
/// `Sized` excludes trait-object dispatch; algorithms are generic type parameters.
pub trait HashProvider: Sized {
    fn hash_parts(&self, parts: &[&[u8]]) -> Hash;

    fn hash(&self, bytes: &[u8]) -> Hash {
        self.hash_parts(&[bytes])
    }
}

/// Configuration identifiers, not hash providers. Match once at startup and
/// enter a generic runner with a concrete provider (see [`crate::HashConfig`]).
/// Unknown names never fall back to Keccak. BLAKE3 uses unkeyed 32-byte output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum HashAlgorithm {
    #[default]
    #[serde(rename = "keccak-256")]
    Keccak256,
    #[serde(rename = "blake3")]
    Blake3,
}

/// Keccak-256 with its original padding (not SHA3-256).
#[derive(Clone, Copy, Debug, Default)]
pub struct Keccak256Hasher;

impl HashProvider for Keccak256Hasher {
    fn hash_parts(&self, parts: &[&[u8]]) -> Hash {
        let mut digest = Keccak256::new();
        for part in parts {
            digest.update(part);
        }
        digest.finalize().into()
    }
}

/// Unkeyed BLAKE3 with the standard 32-byte digest. No key or derive-key context.
#[derive(Clone, Copy, Debug, Default)]
pub struct Blake3Hasher;

impl HashProvider for Blake3Hasher {
    fn hash_parts(&self, parts: &[&[u8]]) -> Hash {
        let mut digest = blake3::Hasher::new();
        for part in parts {
            digest.update(part);
        }
        *digest.finalize().as_bytes()
    }
}
