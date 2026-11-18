use crate::{Blake3Hasher, HashAlgorithm, HashConfig, HashProvider, Keccak256Hasher};

fn hex(hash: [u8; 32]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn keccak_known_answers_and_chunking() {
    let hash = Keccak256Hasher;
    // Standard Keccak-256 vectors. SHA3-256 has different padding and fails these.
    assert_eq!(
        hex(hash.hash(b"")),
        "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
    );
    assert_eq!(
        hex(hash.hash(b"abc")),
        "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45"
    );
    assert_eq!(hash.hash_parts(&[b"a", b"", b"bc"]), hash.hash(b"abc"));
    assert_eq!(hash.hash_iter([&b"a"[..], b"", b"bc"]), hash.hash(b"abc"));
    assert_eq!(hash.hash_iter(std::iter::empty()), hash.hash(b""));
    assert_eq!(hash.hash_parts(&[]), hash.hash(b""));
}

#[test]
fn explicit_yaml_and_file_loading() {
    let config = HashConfig::from_yaml("# protocol\nhash_function: keccak-256\n").unwrap();
    assert_eq!(config.hash_function, HashAlgorithm::Keccak256);
    let from_file = HashConfig::load(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../config/hash.yaml"
    ))
    .unwrap();
    assert_eq!(from_file, config);
    assert!(HashConfig::load(concat!(env!("CARGO_MANIFEST_DIR"), "/no-such-config.yaml")).is_err());
}

#[test]
fn configuration_never_silently_falls_back() {
    for yaml in [
        "",
        "{}",
        "hash_function: sha3-256",
        "hash_function: unknown",
        "hash_function: null",
        "hash_function: 1",
        "hash_functon: keccak-256",
        "hash_function: keccak-256\nextra: true",
        "hash_function: [",
        "hash_function: keccak-256\nhash_function: keccak-256",
        "hash_function: keccak-256\n---\nhash_function: keccak-256",
    ] {
        assert!(
            HashConfig::from_yaml(yaml).is_err(),
            "unexpectedly accepted {yaml:?}"
        );
    }
}

#[test]
fn blake3_known_answers_and_chunking() {
    let hash = Blake3Hasher;
    assert_eq!(
        hex(hash.hash(b"")),
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    );
    assert_eq!(
        hex(hash.hash(b"abc")),
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );
    assert_eq!(hash.hash_parts(&[]), hash.hash(b""));
    assert_eq!(hash.hash_parts(&[b"a", b"", b"bc"]), hash.hash(b"abc"));
    assert_eq!(hash.hash_iter([&b"a"[..], b"", b"bc"]), hash.hash(b"abc"));
    assert_eq!(hash.hash_iter(std::iter::empty()), hash.hash(b""));
    // Cross compression-block and tree-chunk boundaries with fragmented input.
    let bytes = (0..4097).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    for size in [1, 63, 64, 65, 1023, 1024, 1025] {
        let parts = bytes.chunks(size).collect::<Vec<_>>();
        assert_eq!(hash.hash_parts(&parts), hash.hash(&bytes));
    }
    assert_ne!(hash.hash(b"abc"), Keccak256Hasher.hash(b"abc"));
}

#[test]
fn blake3_configuration_selects_concrete_provider() {
    fn run<H: HashProvider>(hasher: H) -> [u8; 32] {
        hasher.hash(b"abc")
    }
    for (yaml, expected, digest) in [
        (
            "hash_function: keccak-256",
            HashAlgorithm::Keccak256,
            Keccak256Hasher.hash(b"abc"),
        ),
        (
            "hash_function: blake3",
            HashAlgorithm::Blake3,
            Blake3Hasher.hash(b"abc"),
        ),
    ] {
        let config = HashConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.hash_function, expected);
        let actual = match config.hash_function {
            HashAlgorithm::Keccak256 => run(Keccak256Hasher),
            HashAlgorithm::Blake3 => run(Blake3Hasher),
        };
        assert_eq!(actual, digest);
    }
    assert_eq!(
        HashConfig::default().hash_function,
        HashAlgorithm::Keccak256
    );
}
