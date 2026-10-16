use std::collections::BTreeMap;

use golemdb_branch::{CommitId, Head, Roots, head_roots, read_head_state, write_head, write_roots};
use golemdb_cells::{CellChange, CellKey, CellNameRef, CellValue, Cells, reserved, system, tables};
use golemdb_index::Index;
use golemdb_merkle::{Hash, HashAlgorithm, HashProvider, RootRef};
use golemdb_storage::{ReadCursor, ReadTransaction, Table, WriteTransaction};

use crate::{Genesis, OpenConfig, OpenError, OpenInfo, OpenMode, OpenResult};

const SUPERBLOCK: Table = Table("Superblock");
const FORMAT: u32 = 1;
const ROARING: u16 = 1;

fn hash_id(algorithm: HashAlgorithm) -> u16 {
    match algorithm {
        HashAlgorithm::Keccak256 => 1,
        HashAlgorithm::Blake3 => 2,
    }
}

fn initial_cells(config: &Genesis) -> BTreeMap<CellKey, CellValue> {
    let mut cells = BTreeMap::new();
    for record in system::ALL {
        cells.insert(
            CellKey::new(record.id, reserved::KEY),
            CellValue::from_bytes32(record.key),
        );
        cells.insert(
            CellKey::new(system::RECORD_KEYS.id, CellNameRef::raw(&record.key)),
            CellValue::from_u64(record.id),
        );
    }
    let limits = config.cell_limits;
    for (name, value) in [
        (reserved::MAX_CELL_NAME_LEN, limits.max_cell_name_len),
        (reserved::MAX_STR_LEN, limits.max_str_len),
        (reserved::MAX_BYTES_LEN, limits.max_bytes_len),
    ] {
        cells.insert(
            CellKey::new(system::PARAMS.id, name),
            CellValue::from_u32(value),
        );
    }
    cells.insert(
        allocator_key(),
        CellValue::from_u64(system::FIRST_USER_RECORD_ID),
    );
    cells
}

fn allocator_key() -> CellKey {
    CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID)
}

/// Identity is independent of YAML formatting, field order, paths, and backend
/// options. It commits to format IDs and all sorted, length-framed genesis cells.
fn identity(
    config: &Genesis,
    cells: &BTreeMap<CellKey, CellValue>,
    hasher: &impl HashProvider,
) -> Hash {
    let mut bytes = b"golemdb/genesis/v1\0".to_vec();
    bytes.extend_from_slice(&FORMAT.to_be_bytes());
    bytes.extend_from_slice(&hash_id(config.hash_function).to_be_bytes());
    bytes.extend_from_slice(&ROARING.to_be_bytes());
    bytes.extend_from_slice(&(cells.len() as u32).to_be_bytes());
    for (key, value) in cells {
        let key = key.encode();
        bytes.extend_from_slice(&(key.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&key);
        let value = value.encoded_bytes();
        bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
        bytes.extend_from_slice(value);
    }
    hasher.hash(&bytes)
}

pub(crate) fn prepare(
    tx: &mut impl WriteTransaction,
    config: &OpenConfig,
    hasher: &impl HashProvider,
) -> OpenResult<OpenInfo> {
    let cells = initial_cells(&config.genesis);
    let genesis_id = identity(&config.genesis, &cells, hasher);
    if tx.get(SUPERBLOCK, b"head")?.is_none() {
        if !tx.is_empty()? {
            return Err(OpenError::CorruptState(
                "storage contains tables or rows but has no head",
            ));
        }
        if config.mode == OpenMode::ExistingOnly {
            return Err(OpenError::NotInitialized);
        }
        tx.put(SUPERBLOCK, b"format", &FORMAT.to_be_bytes())?;
        tx.put(
            SUPERBLOCK,
            b"hash_fn",
            &hash_id(config.genesis.hash_function).to_be_bytes(),
        )?;
        tx.put(SUPERBLOCK, b"roaring", &ROARING.to_be_bytes())?;
        tx.put(SUPERBLOCK, b"genesis_id", &genesis_id)?;
        let commit_id = CommitId::GENESIS;
        let update = Cells::new(hasher).apply(
            tx,
            commit_id.get(),
            RootRef::Empty,
            cells
                .into_iter()
                .map(|(key, value)| CellChange::Put { key, value }),
        )?;
        // Every genesis cell is a field, so IndexRoot is the canonical empty root.
        // #roots cannot contain its own roots and has no history entries at commit 0.
        let head = Head {
            commit_id,
            state_root: update.root.hash(hasher),
            index_root: hasher.hash(&[]),
        };
        let roots = Roots {
            state: update.root,
            index: RootRef::Empty,
        };
        write_roots(tx, head.commit_id, &roots)?;
        write_head(tx, &head)?;
        return Ok(OpenInfo::new(true, head, genesis_id));
    }

    let format = u32::from_be_bytes(metadata(tx, b"format")?);
    if format != FORMAT {
        return Err(OpenError::UnsupportedFormat(format));
    }
    let hash = u16::from_be_bytes(metadata(tx, b"hash_fn")?);
    if !matches!(hash, 1 | 2) {
        return Err(OpenError::UnsupportedHash(hash));
    }
    let roaring = u16::from_be_bytes(metadata(tx, b"roaring")?);
    if roaring != ROARING {
        return Err(OpenError::UnsupportedRoaring(roaring));
    }
    let stored_id: Hash = metadata(tx, b"genesis_id")?;
    if hash != hash_id(config.genesis.hash_function) || stored_id != genesis_id {
        return Err(OpenError::GenesisMismatch);
    }
    let head = read_head_state(tx)?;
    validate_state(tx, &head, cells, hasher)?;
    if config.mode == OpenMode::CreateNew {
        return Err(OpenError::AlreadyInitialized);
    }
    Ok(OpenInfo::new(false, head, genesis_id))
}

fn metadata<const N: usize>(tx: &impl ReadTransaction, key: &[u8]) -> OpenResult<[u8; N]> {
    tx.get(SUPERBLOCK, key)?
        .ok_or(OpenError::CorruptState(
            "missing format or genesis metadata",
        ))?
        .try_into()
        .map_err(|_| OpenError::CorruptState("malformed format or genesis metadata"))
}

fn validate_state(
    tx: &mut impl WriteTransaction,
    head: &Head,
    mut expected: BTreeMap<CellKey, CellValue>,
    hasher: &impl HashProvider,
) -> OpenResult<()> {
    let cells = Cells::new(hasher);
    let roots = head_roots(tx, head, hasher)?;
    let root = cells.reopen(tx, roots.state)?;
    Index::new(hasher).reopen(tx, roots.index)?;
    let allocator = cells
        .get(tx, &allocator_key())?
        .ok_or(OpenError::CorruptState("missing allocator"))?;
    let next_id = allocator
        .as_u64()
        .filter(|_| !allocator.is_indexable())
        .filter(|id| *id >= system::FIRST_USER_RECORD_ID)
        .ok_or(OpenError::CorruptState("invalid allocator"))?;
    if head.commit_id == crate::CommitId::GENESIS && next_id != system::FIRST_USER_RECORD_ID {
        return Err(OpenError::CorruptState("genesis allocator has advanced"));
    }
    if cells
        .scan_record(tx, next_id)?
        .next()
        .transpose()?
        .is_some()
    {
        return Err(OpenError::CorruptState(
            "allocator points to an occupied record",
        ));
    }
    expected.insert(allocator_key(), allocator);
    for (key, value) in &expected {
        if cells.get(tx, key)?.as_ref() != Some(value) {
            return Err(OpenError::CorruptState(
                "missing or inconsistent reserved identity, binding, or parameter",
            ));
        }
    }
    if head.commit_id == crate::CommitId::GENESIS {
        let mut cursor = tx.cursor(tables::CELL, b"")?;
        for _ in 0..expected.len() {
            cursor
                .next()?
                .ok_or(OpenError::CorruptState("incomplete genesis cells"))?;
        }
        if cursor.next()?.is_some() || head.index_root != hasher.hash(&[]) {
            return Err(OpenError::CorruptState("unexpected data at genesis"));
        }
    } else {
        let key = CellKey::new(
            system::ROOTS.id,
            CellNameRef::raw(&(head.commit_id.get() - 1).to_be_bytes()),
        );
        let value = cells
            .get(tx, &key)?
            .ok_or(OpenError::CorruptState("missing previous commit roots"))?;
        if value.is_indexable() || value.as_bytes().is_none_or(|bytes| bytes.len() != 64) {
            return Err(OpenError::CorruptState("malformed previous commit roots"));
        }
        expected.insert(key, value);
    }
    // Applying identical values verifies these required flat cells against their
    // committed trie paths. This performs no writes and does not rebuild genesis.
    cells.apply(
        tx,
        head.commit_id.get(),
        root,
        expected
            .into_iter()
            .map(|(key, value)| CellChange::Put { key, value }),
    )?;
    Ok(())
}
