use std::collections::BTreeMap;

use crate::{BranchError, overlay::CellOverlay};
use golemdb_cells::{CellChange, CellKey, CellNameRef, CellType, CellValue, CellValueRef, tables};
use golemdb_storage::{MemoryStore, Store, WriteTransaction};
use proptest::prelude::*;

fn key(slot: u8) -> CellKey {
    CellKey::new(
        64 + u64::from(slot / 4),
        CellNameRef::raw(&[b'a' + slot % 4]),
    )
}

fn value(number: u8) -> CellValue {
    CellValueRef::new(CellType::Str, &[b'A' + number], number.is_multiple_of(2))
        .unwrap()
        .into()
}

proptest! {
    // The reference keeps complete logical-state snapshots, independently of
    // overlay representation and inverse journal entries.
    #[test]
    fn operations_and_frames_match_full_snapshot_model(
        actions in prop::collection::vec((0u8..8, 0u8..8, 0u8..20), 0..180)
    ) {
        let db = MemoryStore::new();
        let base = BTreeMap::from([(key(0), value(0)), (key(5), value(1))]);
        let mut tx = db.begin_write().unwrap();
        for (key, value) in &base {
            tx.put(tables::CELL, &key.encode(), value.encoded_bytes()).unwrap();
        }
        tx.commit().unwrap();
        let origin = db.begin_read().unwrap();
        let mut overlay = CellOverlay::new();
        let mut state = base.clone();
        let mut frames = vec![state.clone()];
        let mut open = true;

        for (action, slot, number) in actions {
            match action {
                0..=2 => {
                    if !open {
                        frames.push(state.clone());
                        open = true;
                    }
                    overlay.write(&origin, |cells| {
                        match action {
                            0 => cells.put(key(slot), value(number)),
                            1 => cells.delete(key(slot)),
                            _ => {
                                cells.delete(key(slot));
                                cells.put(key(slot), value(number));
                                cells.put(key((slot + 1) % 8), value((number + 1) % 20));
                            }
                        }
                        Ok::<_, BranchError>(())
                    }).unwrap();
                    match action {
                        0 => { state.insert(key(slot), value(number)); }
                        1 => { state.remove(&key(slot)); }
                        _ => {
                            state.insert(key(slot), value(number));
                            state.insert(key((slot + 1) % 8), value((number + 1) % 20));
                        }
                    }
                }
                3 => {
                    frames.push(state.clone());
                    open = true;
                    overlay.checkpoint();
                }
                4 => match frames.pop() {
                    Some(previous) => {
                        state = previous;
                        open = false;
                        overlay.rollback().unwrap();
                    }
                    None => prop_assert!(matches!(overlay.rollback(), Err(BranchError::NoFrameToRollback))),
                },
                5 | 6 => {
                    let before = overlay.changes().collect::<Vec<_>>();
                    let result = overlay.write(&origin, |cells| {
                        cells.put(key(slot), value(number));
                        cells.delete(key((slot + 1) % 8));
                        cells.put(key(slot), value((number + 1) % 20));
                        Err::<(), _>("abort")
                    });
                    prop_assert_eq!(result, Err("abort"));
                    prop_assert_eq!(overlay.changes().collect::<Vec<_>>(), before);
                }
                _ => {
                    let read = overlay.write(&origin, |cells| cells.get(&key(slot))).unwrap();
                    prop_assert_eq!(read, state.get(&key(slot)).cloned());
                }
            }

            for slot in 0..8 {
                prop_assert_eq!(overlay.get(&origin, &key(slot)).unwrap(), state.get(&key(slot)).cloned());
            }
            for record in [64u64, 65] {
                let actual = overlay.scan_prefix(&origin, &record.to_be_bytes()).unwrap()
                    .collect::<crate::Result<Vec<_>>>().unwrap();
                let expected = state.iter().filter(|(key, _)| key.record_id() == record)
                    .map(|(key, value)| (key.clone(), value.clone())).collect::<Vec<_>>();
                prop_assert_eq!(actual, expected);
            }
            let mut applied = base.clone();
            for change in overlay.changes() {
                match change {
                    CellChange::Put { key, value } => { applied.insert(key, value); }
                    CellChange::Delete { key } => { applied.remove(&key); }
                }
            }
            prop_assert_eq!(&applied, &state);
        }
    }
}
