use golemdb_branch::{BranchId, Branches};
use golemdb_cells::{CellKey, CellKind, CellLimits, CellNameRef, CellValue, reserved, system};
use golemdb_merkle::Keccak256Hasher;
use golemdb_metering::{
    Budget, FailureReason, ModeledDepths, Schedule, WRITE_CODE_VERSION, WeightName as W,
};
use golemdb_storage::{Entry, MemoryStore, ReadCursor, ReadTransaction, Store, Table};
use std::sync::{Arc, Mutex};

use crate::{
    CreateEntry, Effects, PatchChange, PatchEntry, ReadTarget, RecordError, RecordKey,
    RecordLimits, Records, WritePricing, crud_tests,
};

const KEY: RecordKey = RecordKey([0x42; 32]);
const OTHER: RecordKey = RecordKey([0x43; 32]);

fn pricing() -> WritePricing {
    WritePricing {
        provider: crud_tests::pricing_provider(
            Schedule::new(
                WRITE_CODE_VERSION,
                [
                    (W::READ_ADMISSION_BASE, 0),
                    (W::READ_ADMISSION_ENTRY, 0),
                    (W::READ_ADMISSION_BYTE, 0),
                    (W::READ_BASE, 0),
                    (W::READ_CELL_SCAN, 0),
                    (W::READ_BYTE, 0),
                    (W::WRITE_ADMISSION_BASE, 2),
                    (W::WRITE_ADMISSION_ENTRY, 1),
                    (W::WRITE_ADMISSION_BYTE, 1),
                    (W::READ_KEY, 1),
                    (W::WRITE_RECORD_CREATE, 2),
                    (W::WRITE_RECORD_PATCH, 2),
                    (W::WRITE_RECORD_DELETE, 2),
                    (W::READ_CELL, 2),
                    (W::READ_TERM, 2),
                    (W::WRITE_CELL_CREATE, 5),
                    (W::WRITE_CELL_UPDATE, 6),
                    (W::WRITE_CELL_DELETE, 4),
                    (W::WRITE_CELL_TRIE_UPDATE, 1),
                    (W::WRITE_CELL_BYTE, 1),
                    (W::WRITE_INDEX_JOIN, 3),
                    (W::WRITE_INDEX_LEAVE, 2),
                    (W::WRITE_INDEX_TRIE_UPDATE, 1),
                    (W::WRITE_INDEX_TERM_CREATE, 7),
                    (W::WRITE_INDEX_BYTE, 1),
                ],
            )
            .unwrap(),
        ),
        depths: ModeledDepths {
            cell_trie: 4,
            index_trie: 4,
        },
        cell_limits: CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
        record_limits: RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
    }
}

fn entry(name: &[u8], value: CellValue, kind: CellKind) -> CreateEntry {
    CreateEntry {
        name: CellNameRef::raw(name).into(),
        value,
        kind,
    }
}

fn entries() -> Vec<CreateEntry> {
    vec![
        entry(b"age", CellValue::from_u32(7), CellKind::Field),
        entry(b"color", CellValue::from_str("red"), CellKind::Attribute),
    ]
}

fn patch(name: &[u8], change: PatchChange) -> Vec<PatchEntry> {
    vec![PatchEntry {
        name: CellNameRef::raw(name).into(),
        change,
    }]
}

fn snapshot<S: Store>(
    branches: &Branches<S, Keccak256Hasher>,
    branch: BranchId,
) -> Vec<(CellKey, CellValue)> {
    branches
        .read(branch, |r| {
            r.scan_prefix(&[])?.collect::<golemdb_branch::Result<_>>()
        })
        .unwrap()
}

#[test]
fn real_create_matches_the_worked_example_and_keeps_system_charges_out_of_effects() {
    let (_, branches, _) = crud_tests::setup();
    let snapshot = pricing().provider.snapshot(0).unwrap();
    let schedule = &snapshot.model.schedule;
    let records = Records::new(branches.clone(), pricing());
    let branch = branches.begin().unwrap();
    let result = records.create(branch, KEY, entries(), Budget::Limited(266));
    assert_eq!(result.result.unwrap(), KEY);
    assert_eq!(result.accounting.cost, 266);
    assert_eq!(schedule.price(&result.accounting.ledger).unwrap(), 266);
    assert_eq!(result.accounting.ledger.count("write.cell.create"), 5);
    assert_eq!(result.accounting.ledger.count("write.cell_byte"), 173);
    assert_eq!(result.accounting.ledger.count("read.cell"), 0);
    assert_eq!(result.effects.cells_created, 2);
    assert_eq!(result.effects.cell_bytes_written, 33);
    assert_eq!(result.effects.index_joins, 1);
    let record = records
        .get(ReadTarget::Branch(branch), KEY, None, Budget::Unlimited)
        .result
        .unwrap();
    assert_eq!(record.meta().unwrap().cells, 2);
    branches.commit(branch).unwrap();
}

#[test]
fn every_create_checkpoint_rejects_without_writes_or_id_consumption() {
    for (budget, spent, required) in [
        (0, 0, None),
        (1, 0, None),
        (2, 2, None),
        (3, 3, None),
        (9, 3, None),
        (10, 10, None),
        (11, 11, None),
        (18, 11, None),
        (19, 19, None),
        (21, 19, None),
        (22, 22, None),
        (23, 22, None),
        (24, 24, Some(266)),
        (265, 24, Some(266)),
    ] {
        let (_, branches, _) = crud_tests::setup();
        let records = Records::new(branches.clone(), pricing());
        let branch = branches.begin().unwrap();
        let before = snapshot(&branches, branch);
        let version = branches.branch_info(branch).unwrap().version;
        let outcome = records.create(branch, KEY, entries(), Budget::Limited(budget));
        let Err(RecordError::Metering(failure)) = outcome.result else {
            panic!("budget {budget}")
        };
        assert_eq!(failure.reason, FailureReason::BudgetExceeded);
        assert_eq!(failure.required, required, "budget {budget}");
        assert_eq!(outcome.accounting, failure.accounting);
        assert_eq!(outcome.accounting.cost, spent, "budget {budget}");
        assert_eq!(outcome.effects, Effects::default());
        assert_eq!(outcome.accounting.ledger.count("write.cell.create"), 0);
        assert_eq!(snapshot(&branches, branch), before);
        assert_eq!(branches.branch_info(branch).unwrap().version, version);
        records
            .create(branch, KEY, entries(), Budget::Unlimited)
            .result
            .unwrap();
        let next = branches
            .read(branch, |r| {
                r.get(&CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID))
            })
            .unwrap()
            .unwrap();
        assert_eq!(next.as_u64(), Some(65));
    }
}

#[test]
fn rejected_admission_pays_only_the_reached_checks_in_input_order() {
    let (_, branches, _) = crud_tests::setup();
    let records = Records::new(branches.clone(), pricing());
    let branch = branches.begin().unwrap();
    let bad_name = entry(b"!bad", CellValue::from_bool(true), CellKind::Field);
    let outcome = records.create(branch, KEY, vec![bad_name], Budget::Unlimited);
    assert!(matches!(
        outcome.result,
        Err(RecordError::InvalidInput { .. })
    ));
    assert_eq!(outcome.accounting.cost, 8); // base 2 + entry 1 + bytes 5
    assert_eq!(outcome.accounting.ledger.count("read.key"), 0);
    let overlong = entry(&[b'a'; 33], CellValue::from_bool(true), CellKind::Field);
    let outcome = records.create(branch, KEY, vec![overlong], Budget::Unlimited);
    assert_eq!(outcome.accounting.cost, 3);
    assert_eq!(outcome.accounting.ledger.count("write.admission.byte"), 0);
    let reserved = RecordKey(system::PARAMS.key);
    let outcome = records.create(branch, reserved, entries(), Budget::Unlimited);
    assert!(matches!(outcome.result, Err(RecordError::Reserved)));
    assert_eq!(outcome.accounting.cost, 2);
    let duplicate = entry(b"age", CellValue::from_u32(8), CellKind::Field);
    let outcome = records.create(
        branch,
        KEY,
        vec![entries().remove(0), duplicate],
        Budget::Unlimited,
    );
    assert!(matches!(
        outcome.result,
        Err(RecordError::InvalidInput { .. })
    ));
    assert_eq!(outcome.accounting.cost, 18); // base 2 + twice (entry 1 + bytes 7)
    assert_eq!(outcome.accounting.ledger.count("read.key"), 0);
}

#[test]
fn patch_entry_count_can_exceed_record_caps_without_growing_the_record() {
    let (_, branches, _) = crud_tests::setup();
    let records = Records::new(branches.clone(), pricing());
    let branch = branches.begin().unwrap();
    records
        .create(branch, KEY, vec![], Budget::Unlimited)
        .result
        .unwrap();
    let version = branches.branch_info(branch).unwrap().version;
    // Exceed the former request cap; absent removals leave the empty record intact.
    let entries = (0..65_537)
        .map(|i| PatchEntry {
            name: CellNameRef::raw(format!("missing{i}").as_bytes()).into(),
            change: PatchChange::Remove,
        })
        .collect();
    let result = records.patch(branch, KEY, entries, Budget::Unlimited);
    result.result.unwrap();
    assert_eq!(
        result.accounting.ledger.count("write.admission.entry"),
        65_537
    );
    assert_eq!(result.accounting.ledger.count("read.cell"), 65_539);
    assert_eq!(result.effects, Effects::default());
    assert_eq!(branches.branch_info(branch).unwrap().version, version);
}

#[test]
fn admission_uses_configured_cell_lengths_without_a_total_request_byte_cap() {
    let (_, branches, _) = crud_tests::setup();
    let mut p = pricing();
    let payload_len = 16 * 1024 * 1024;
    p.cell_limits.max_bytes_len = payload_len;
    let records = Records::new(branches.clone(), p);
    let branch = branches.begin().unwrap();
    // The payload fits the configured limit; its name exceeds the former total cap.
    let big = entry(
        b"a",
        CellValue::from_bytes(&vec![0; payload_len as usize]),
        CellKind::Field,
    );
    let result = records.create(branch, KEY, vec![big], Budget::Unlimited);
    result.result.unwrap();
    assert_eq!(
        result.accounting.ledger.count("write.admission.byte"),
        u64::from(payload_len) + 1
    );
    assert_eq!(result.effects.cells_created, 1);
}

#[test]
fn patch_prices_current_branch_values_and_skips_unchanged_mutations() {
    let (_, branches, _) = crud_tests::setup();
    let records = Records::new(branches.clone(), pricing());
    let branch = branches.begin().unwrap();
    records
        .create(branch, KEY, entries(), Budget::Unlimited)
        .result
        .unwrap();
    let version = branches.branch_info(branch).unwrap().version;
    let noop = records.patch(
        branch,
        KEY,
        patch(b"color", PatchChange::Set(CellValue::from_str("red"))),
        Budget::Unlimited,
    );
    noop.result.unwrap();
    assert_eq!(noop.accounting.cost, 20); // admission 11 + base 3 + meta/key/cell reads 6
    assert_eq!(noop.accounting.ledger.count("read.cell"), 3);
    assert_eq!(noop.accounting.ledger.count("read.term"), 0);
    assert_eq!(noop.effects, Effects::default());
    assert_eq!(branches.branch_info(branch).unwrap().version, version);
    let changed = records.patch(
        branch,
        KEY,
        patch(b"color", PatchChange::Set(CellValue::from_str("tan"))),
        Budget::Unlimited,
    );
    changed.result.unwrap();
    assert_eq!(changed.accounting.ledger.count("read.term"), 2);
    assert_eq!(changed.accounting.ledger.count("write.cell.update"), 1); // equal-size #meta stays unchanged
    assert_eq!(changed.accounting.ledger.count("write.index.leave"), 1);
    assert_eq!(changed.accounting.ledger.count("write.index.join"), 1);
    assert_eq!(changed.effects.cells_updated, 1);
    let miss = records.patch(
        branch,
        KEY,
        patch(b"missing", PatchChange::Set(CellValue::from_bool(true))),
        Budget::Unlimited,
    );
    assert!(matches!(miss.result, Err(RecordError::CellNotFound { .. })));
    assert_eq!(miss.accounting.ledger.count("write.cell.update"), 0);
    assert_eq!(miss.effects, Effects::default());
}

#[test]
fn caps_fail_without_charging_prospective_mutations() {
    let (_, branches, _) = crud_tests::setup();
    let mut p = pricing();
    p.record_limits.max_cells = 1;
    let records = Records::new(branches.clone(), p);
    let branch = branches.begin().unwrap();
    let before = snapshot(&branches, branch);
    let result = records.create(branch, KEY, entries(), Budget::Unlimited);
    assert!(matches!(
        result.result,
        Err(RecordError::InvalidInput { .. })
    ));
    assert_eq!(result.accounting.cost, 24);
    assert_eq!(result.accounting.ledger.count("write.cell.create"), 0);
    assert_eq!(snapshot(&branches, branch), before);
}

#[test]
fn term_creation_uses_the_origin_even_after_an_earlier_branch_join() {
    let (_, branches, _) = crud_tests::setup();
    let records = Records::new(branches.clone(), pricing());
    let branch = branches.begin().unwrap();
    for key in [KEY, OTHER] {
        let outcome = records.create(branch, key, entries(), Budget::Unlimited);
        outcome.result.unwrap();
        assert_eq!(
            outcome.accounting.ledger.count("write.index_term_create"),
            1
        );
    }
    branches.commit(branch).unwrap();
    let branch = branches.begin().unwrap();
    let outcome = records.create(branch, RecordKey([0x44; 32]), entries(), Budget::Unlimited);
    outcome.result.unwrap();
    assert_eq!(
        outcome.accounting.ledger.count("write.index_term_create"),
        0
    );
    assert_eq!(outcome.accounting.cost, 259);
}

#[test]
fn invalid_and_sealed_handles_have_no_accounting() {
    let (_, branches, _) = crud_tests::setup();
    let records = Records::new(branches.clone(), pricing());
    let branch = branches.begin().unwrap();
    branches.seal(branch).unwrap();
    let result = records.create(branch, KEY, entries(), Budget::Unlimited);
    assert!(result.result.is_err());
    assert_eq!(result.priced_at, None);
    assert_eq!(result.accounting.cost, 0);
    assert!(result.accounting.ledger.is_empty());
    branches.discard(branch).unwrap();
    let result = records.delete(branch, KEY, Budget::Unlimited);
    assert_eq!(result.priced_at, None);
    assert!(result.accounting.ledger.is_empty());
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Access {
    Get(Table, Vec<u8>),
    Inspect(Table, Vec<u8>),
    Cursor(Table, Vec<u8>),
    Next(Table),
}

struct ObservedStore {
    inner: MemoryStore,
    accesses: Arc<Mutex<Vec<Access>>>,
}
struct ObservedRead<R> {
    inner: R,
    accesses: Arc<Mutex<Vec<Access>>>,
}
struct ObservedCursor<C> {
    inner: C,
    table: Table,
    accesses: Arc<Mutex<Vec<Access>>>,
}
impl Store for ObservedStore {
    type Read<'a> = ObservedRead<<MemoryStore as Store>::Read<'a>>;
    type Write<'a> = <MemoryStore as Store>::Write<'a>;
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        Ok(ObservedRead {
            inner: self.inner.begin_read()?,
            accesses: self.accesses.clone(),
        })
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        self.inner.begin_write()
    }
}
impl<R: ReadTransaction> ReadTransaction for ObservedRead<R> {
    type Cursor<'a>
        = ObservedCursor<R::Cursor<'a>>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        self.accesses
            .lock()
            .unwrap()
            .push(Access::Get(table, key.to_vec()));
        self.inner.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.accesses
            .lock()
            .unwrap()
            .push(Access::Cursor(table, key.to_vec()));
        Ok(ObservedCursor {
            inner: self.inner.cursor(table, key)?,
            table,
            accesses: self.accesses.clone(),
        })
    }

    fn with_value<Output>(
        &self,
        table: golemdb_storage::Table,
        key: &[u8],
        inspect: impl FnOnce(Option<&[u8]>) -> Output,
    ) -> golemdb_storage::Result<Output> {
        self.accesses
            .lock()
            .unwrap()
            .push(Access::Inspect(table, key.to_vec()));
        self.inner.with_value(table, key, inspect)
    }
}
impl<C: ReadCursor> ReadCursor for ObservedCursor<C> {
    fn next(&mut self) -> golemdb_storage::Result<Option<Entry>> {
        self.accesses.lock().unwrap().push(Access::Next(self.table));
        self.inner.next()
    }
    fn prev(&mut self) -> golemdb_storage::Result<Option<Entry>> {
        self.inner.prev()
    }

    fn with_next<Output>(
        &mut self,
        inspect: impl FnOnce(Option<(&[u8], &[u8])>) -> Output,
    ) -> golemdb_storage::Result<Output> {
        self.accesses.lock().unwrap().push(Access::Next(self.table));
        self.inner.with_next(inspect)
    }
}

#[test]
fn admission_and_unfunded_reads_do_not_access_record_or_index_state() {
    for budget in [0, 19, 21, 22, 23, 24, 265, 266] {
        let (store, _, _) = crud_tests::setup();
        let accesses = Arc::new(Mutex::new(Vec::new()));
        let branches = Branches::new(
            ObservedStore {
                inner: store,
                accesses: accesses.clone(),
            },
            Keccak256Hasher,
            None,
            None,
        )
        .unwrap();
        let records = Records::new(branches.clone(), pricing());
        let branch = branches.begin().unwrap();
        accesses.lock().unwrap().clear();
        let result = records.create(branch, KEY, entries(), Budget::Limited(budget));
        assert_eq!(result.result.is_ok(), budget == 266);
        let calls = accesses.lock().unwrap();
        let cell_gets: Vec<_> = calls
            .iter()
            .filter_map(|call| match call {
                Access::Get(table, key) if *table == golemdb_cells::tables::CELL => {
                    Some(key.clone())
                }
                _ => None,
            })
            .collect();
        let expected = if budget >= 22 {
            vec![
                crate::state::binding(KEY).encode(),
                crate::state::allocator_key().encode(),
            ]
        } else {
            vec![]
        };
        assert_eq!(cell_gets, expected, "budget {budget}");
        let index_gets = calls
            .iter()
            .filter(|call| matches!(call, Access::Get(Table("Index"), _)))
            .count();
        assert_eq!(index_gets, usize::from(budget >= 24), "budget {budget}");
        assert!(!calls.iter().any(|call| matches!(
            call,
            Access::Get(Table("BitmapContainer" | "BitmapTrie"), _)
        )));
        // Success and final refusal perform exactly the same planning reads;
        // application itself adds no physical reads.
    }
}

#[test]
fn patch_cutoffs_stop_at_metadata_identity_and_user_cell_boundaries() {
    for (budget, expected_names) in [
        (14, vec![]),
        (16, vec!["#meta"]),
        (18, vec!["#meta", "#key"]),
        (44, vec!["#meta", "#key", "age"]),
        (45, vec!["#meta", "#key", "age"]),
    ] {
        let (store, base_branches, base_records) = crud_tests::setup();
        let base = base_branches.begin().unwrap();
        base_records
            .create(base, KEY, entries(), Budget::Unlimited)
            .result
            .unwrap();
        base_branches.commit(base).unwrap();
        let accesses = Arc::new(Mutex::new(Vec::new()));
        let branches = Branches::new(
            ObservedStore {
                inner: store,
                accesses: accesses.clone(),
            },
            Keccak256Hasher,
            None,
            None,
        )
        .unwrap();
        let records = Records::new(branches.clone(), pricing());
        let branch = branches.begin().unwrap();
        let before = snapshot(&branches, branch);
        accesses.lock().unwrap().clear();
        let outcome = records.patch(
            branch,
            KEY,
            patch(b"age", PatchChange::Set(CellValue::from_u32(8))),
            Budget::Limited(budget),
        );
        let user_reads: Vec<_> = accesses
            .lock()
            .unwrap()
            .iter()
            .filter_map(|call| match call {
                Access::Get(table, key)
                    if *table == golemdb_cells::tables::CELL
                        && key.starts_with(&64u64.to_be_bytes()) =>
                {
                    Some(String::from_utf8(key[8..].to_vec()).unwrap())
                }
                _ => None,
            })
            .collect();
        assert_eq!(user_reads, expected_names, "budget {budget}");
        if budget == 45 {
            outcome.result.unwrap();
            assert_eq!(outcome.accounting.cost, 45);
        } else {
            let Err(RecordError::Metering(failure)) = outcome.result else {
                panic!("budget {budget}")
            };
            assert_eq!(failure.required, if budget == 44 { Some(45) } else { None });
            assert_eq!(outcome.effects, Effects::default());
            assert_eq!(snapshot(&branches, branch), before);
            assert_eq!(branches.branch_info(branch).unwrap().version, 0);
        }
    }
}

#[test]
fn delete_counts_each_live_cell_once_and_charges_no_deleted_bytes() {
    for committed in [false, true] {
        for (budget, spent, required) in [
            (6, 5, None),
            (8, 7, None),
            (10, 9, None),
            (12, 11, None),
            (14, 13, None),
            (15, 15, Some(61)),
            (60, 15, Some(61)),
            (61, 61, None),
        ] {
            let (_, branches, _) = crud_tests::setup();
            let records = Records::new(branches.clone(), pricing());
            let mut branch = branches.begin().unwrap();
            records
                .create(branch, KEY, entries(), Budget::Unlimited)
                .result
                .unwrap();
            if committed {
                branches.commit(branch).unwrap();
                branch = branches.begin().unwrap();
            }
            branches.checkpoint(branch).unwrap();
            let before = snapshot(&branches, branch);
            let info = branches.branch_info(branch).unwrap();
            let outcome = records.delete(branch, KEY, Budget::Limited(budget));
            assert_eq!(outcome.accounting.cost, spent, "{committed}, {budget}");
            assert_eq!(outcome.accounting.ledger.count("write.cell_byte"), 0);
            assert_eq!(outcome.accounting.ledger.count("write.index_byte"), 0);
            if budget == 61 {
                outcome.result.unwrap();
                assert_eq!(outcome.accounting.ledger.count("read.cell"), 4); // #meta, #key, two user cells
                assert_eq!(outcome.accounting.ledger.count("read.term"), 1);
                assert_eq!(outcome.accounting.ledger.count("write.cell.delete"), 5);
                assert_eq!(outcome.effects.cells_deleted, 2);
                assert!(matches!(
                    records
                        .get(ReadTarget::Branch(branch), KEY, None, Budget::Unlimited)
                        .result,
                    Err(RecordError::NotFound)
                ));
                branches.rollback(branch).unwrap();
                assert_eq!(snapshot(&branches, branch), before);
                // The restored scan index must support another priced delete.
                records
                    .delete(branch, KEY, Budget::Limited(61))
                    .result
                    .unwrap();
                branches.commit(branch).unwrap();
            } else {
                let Err(RecordError::Metering(failure)) = outcome.result else {
                    panic!("budget {budget}")
                };
                assert_eq!(failure.required, required);
                assert_eq!(outcome.effects, Effects::default());
                assert_eq!(outcome.accounting.ledger.count("write.cell.delete"), 0);
                assert_eq!(snapshot(&branches, branch), before);
                assert_eq!(branches.branch_info(branch).unwrap(), info);
            }
        }
    }
}

#[test]
fn deletion_skips_a_long_tombstone_run_without_extra_charges_or_unfunded_steps() {
    for budget in [10, 11, 43] {
        let (store, base_branches, base_records) = crud_tests::setup();
        let base = base_branches.begin().unwrap();
        let many = (0..100)
            .map(|n| {
                entry(
                    format!("a{n:04}").as_bytes(),
                    CellValue::from_u32(n),
                    CellKind::Field,
                )
            })
            .collect();
        base_records
            .create(base, KEY, many, Budget::Unlimited)
            .result
            .unwrap();
        base_branches.commit(base).unwrap();
        let accesses = Arc::new(Mutex::new(Vec::new()));
        let branches = Branches::new(
            ObservedStore {
                inner: store,
                accesses: accesses.clone(),
            },
            Keccak256Hasher,
            None,
            None,
        )
        .unwrap();
        let records = Records::new(branches.clone(), pricing());
        let branch = branches.begin().unwrap();
        let removals = (0..99)
            .map(|n| patch(format!("a{n:04}").as_bytes(), PatchChange::Remove).remove(0))
            .collect();
        records
            .patch(branch, KEY, removals, Budget::Unlimited)
            .result
            .unwrap();
        accesses.lock().unwrap().clear();
        let result = records.delete(branch, KEY, Budget::Limited(budget));
        let steps = accesses
            .lock()
            .unwrap()
            .iter()
            .filter(|call| matches!(call, Access::Next(_)))
            .count();
        if budget == 10 {
            assert_eq!(steps, 0); // no user-cell checkpoint was funded
            assert_eq!(result.accounting.cost, 9);
        } else {
            assert!(
                steps <= 5,
                "{steps} physical steps for one remaining user cell"
            );
            assert_eq!(result.accounting.ledger.count("read.cell"), 3);
            assert_eq!(result.accounting.cost, if budget == 43 { 43 } else { 11 });
        }
        assert_eq!(result.result.is_ok(), budget == 43);
    }
}

#[test]
fn empty_records_missing_records_and_zero_weights_have_defined_charges() {
    let (_, branches, _) = crud_tests::setup();
    let records = Records::new(branches.clone(), pricing());
    let branch = branches.begin().unwrap();
    let missing = records.delete(branch, KEY, Budget::Unlimited);
    assert!(matches!(missing.result, Err(RecordError::NotFound)));
    assert_eq!(missing.accounting.cost, 5);
    let empty = records.create(branch, KEY, vec![], Budget::Unlimited);
    empty.result.unwrap();
    assert_eq!(empty.accounting.cost, 172); // 5 + 3*9 + 140 system bytes
    let duplicate = records.create(branch, KEY, vec![], Budget::Unlimited);
    assert!(matches!(duplicate.result, Err(RecordError::AlreadyExists)));
    assert_eq!(duplicate.accounting.cost, 5);
    let deleted = records.delete(branch, KEY, Budget::Limited(33));
    deleted.result.unwrap();
    assert_eq!(deleted.accounting.cost, 33);
    assert_eq!(deleted.effects, Effects::default());

    let mut p = pricing();
    p.provider = crud_tests::pricing_provider(Schedule::zero_write());
    let records = Records::new(branches.clone(), p);
    let created = records.create(branch, KEY, entries(), Budget::Limited(0));
    created.result.unwrap();
    assert_eq!(created.accounting.cost, 0);
    assert_eq!(created.accounting.ledger.count("write.cell.create"), 5);
    records
        .delete(branch, KEY, Budget::Limited(0))
        .result
        .unwrap();
}

#[test]
fn get_stops_before_unpaid_storage_work_and_skips_user_tombstone_runs() {
    let (store, _, _) = crud_tests::setup();
    let accesses = Arc::new(Mutex::new(Vec::new()));
    let branches = Branches::new(
        ObservedStore {
            inner: store,
            accesses: accesses.clone(),
        },
        Keccak256Hasher,
        None,
        None,
    )
    .unwrap();
    let setup = Records::new(branches.clone(), crud_tests::zero_pricing());
    let b = branches.begin().unwrap();
    setup
        .create(
            b,
            KEY,
            (0..40)
                .map(|i| {
                    entry(
                        format!("x{i:02}").as_bytes(),
                        CellValue::from_u32(i),
                        CellKind::Field,
                    )
                })
                .collect(),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    branches.commit(b).unwrap();
    let b = branches.begin().unwrap();
    setup
        .patch(
            b,
            KEY,
            (0..39)
                .map(|i| PatchEntry {
                    name: CellNameRef::raw(format!("x{i:02}").as_bytes()).into(),
                    change: PatchChange::Remove,
                })
                .collect(),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    let mut config = crud_tests::zero_pricing();
    let mut weights = golemdb_metering::PricingConfig::zero().weights;
    for name in ["read.key", "read.cell", "read.cell_scan", "read.byte"] {
        weights.insert(name.into(), 1);
    }
    config.provider = crud_tests::pricing_provider(
        Schedule::new(
            WRITE_CODE_VERSION,
            weights
                .into_iter()
                .map(|(name, price)| (name.into(), price)),
        )
        .unwrap(),
    );
    let records = Records::new(branches.clone(), config);
    // The surviving x39 is committed; projections must use borrowed access.
    let projection = [CellNameRef::raw(b"x39").into()];
    for (budget, inspected) in [(1, false), (2, true)] {
        accesses.lock().unwrap().clear();
        let call = records.get(
            ReadTarget::Branch(b),
            KEY,
            Some(&projection),
            Budget::Limited(budget),
        );
        assert!(matches!(call.result, Err(RecordError::Metering(_))));
        assert_eq!(call.accounting.cost, budget);
        let access = accesses.lock().unwrap();
        let cell = CellKey::new(64, CellNameRef::raw(b"x39")).encode();
        assert_eq!(
            access.contains(&Access::Inspect(golemdb_cells::tables::CELL, cell.clone())),
            inspected
        );
        assert!(!access.contains(&Access::Get(golemdb_cells::tables::CELL, cell)));
        assert_eq!(call.accounting.ledger.count("read.byte"), 0);
    }
    accesses.lock().unwrap().clear();
    let refused = records.get(ReadTarget::Branch(b), KEY, None, Budget::Limited(3));
    assert!(matches!(refused.result, Err(RecordError::Metering(_))));
    assert!(
        !accesses
            .lock()
            .unwrap()
            .iter()
            .any(|a| matches!(a, Access::Cursor(_, _)))
    );
    accesses.lock().unwrap().clear();
    let full = records.get(ReadTarget::Branch(b), KEY, None, Budget::Unlimited);
    assert_eq!(full.result.unwrap().cells.len(), 3);
    assert_eq!(full.accounting.ledger.count("read.cell_scan"), 1);
    assert!(
        accesses
            .lock()
            .unwrap()
            .iter()
            .filter(|a| matches!(a, Access::Next(_)))
            .count()
            <= 2
    );
}
