use std::{hint::black_box, ops::Bound, time::Instant};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use golemdb_api::*;

/// Four containers' worth of record IDs, so a term's postings can span
/// several containers under a branch.
const RECORDS: u64 = 4 << 16;
/// Creates per commit; it barely matters to warm reads, and big commits keep
/// the setup short.
const PER_COMMIT: u64 = 1_000;
/// `skewed` values: `big`, then `small-1` to `small-49`.
const SKEWED: u64 = 50;
/// `number` values: about five records each, at random positions.
const NUMBERS: u64 = RECORDS / 5;
const BUDGET: Budget = Budget::Unlimited;

/// SplitMix64: deterministic, well-spread values per record.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// A record's indexed values: `quarter` takes 4 values of 25% each; `skewed`
/// takes `big` for about 20% of records and 49 small values of about 1.6%
/// each; `number` takes one of `NUMBERS` values at random.
struct Row {
    quarter: u64,
    skewed: u64,
    number: u64,
}

impl Row {
    fn new(i: u64) -> Self {
        let r = mix(i);
        Self {
            quarter: (r >> 16) % 4,
            skewed: if r % 100 < 20 {
                0
            } else {
                1 + (r >> 8) % (SKEWED - 1)
            },
            number: mix(r) % NUMBERS,
        }
    }
}

fn config() -> OpenConfig {
    OpenConfig::new(Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
        RecordLimits {
            max_cells: 16,
            max_indexed_cells: 16,
        },
        None,
    ))
}

fn seed(db: &Database, rows: &[Row]) {
    let started = Instant::now();
    for start in (0..RECORDS).step_by(PER_COMMIT as usize) {
        let branch = db.begin().unwrap();
        for i in start..(start + PER_COMMIT).min(RECORDS) {
            let row = &rows[i as usize];
            let skewed = match row.skewed {
                0 => "big".to_string(),
                small => format!("small-{small}"),
            };
            let mut key = [0; 32];
            key[..8].copy_from_slice(&i.to_be_bytes());
            let create = RecordOp::create(RecordKey(key), BUDGET)
                .attribute("quarter", CellValue::from_str(&format!("q{}", row.quarter)))
                .attribute("skewed", CellValue::from_str(&skewed))
                .attribute("number", CellValue::from_u64(row.number));
            db.create(branch, create).into_result().unwrap();
        }
        db.commit(branch).unwrap();
    }
    eprintln!("seeded {RECORDS} records in {:.1?}", started.elapsed());
}

fn eq(field: &str, value: &str) -> Filter {
    Filter::Eq {
        field: CellNameRef::raw(field.as_bytes()).into(),
        value: EqValue::of(marker::Str, value),
    }
}

/// A benchmark group whose cases are timed per match, against the size of
/// the whole match set: `LIMIT 10` and `count` cases show what a small page
/// and a bare count cost against it.
struct Group<'a> {
    db: &'a Database,
    group: criterion::BenchmarkGroup<'a, criterion::measurement::WallTime>,
}

impl<'a> Group<'a> {
    fn new(c: &'a mut Criterion, db: &'a Database, name: &str) -> Self {
        let mut group = c.benchmark_group(name);
        group.sample_size(10);
        Self { db, group }
    }

    /// Check that the index counts the `matches` the rows do.
    fn check(&self, name: &str, filter: &Filter, matches: u64) {
        let count = self.db.count(None, CountOp::filter(filter.clone(), BUDGET));
        assert_eq!(count.into_result().unwrap(), matches, "{name}");
    }

    fn time<R>(&mut self, name: &str, matches: u64, mut run: impl FnMut() -> R) {
        self.group.throughput(Throughput::Elements(matches));
        self.group.bench_function(name, |b| b.iter(&mut run));
    }

    fn query(&mut self, name: &str, filter: Filter, limit: Option<u64>, matches: u64) {
        self.check(name, &filter, matches);
        let op = match limit {
            Some(limit) => QueryOp::from_filter(filter, BUDGET).limit(limit),
            None => QueryOp::from_filter(filter, BUDGET),
        };
        let db = self.db;
        self.time(name, matches, || {
            db.query(None, black_box(op.clone())).into_result().unwrap()
        });
    }

    fn count(&mut self, name: &str, filter: Filter, matches: u64) {
        self.check(name, &filter, matches);
        let (db, op) = (self.db, CountOp::filter(filter, BUDGET));
        self.time(name, matches, || {
            db.count(None, black_box(op.clone())).into_result().unwrap()
        });
    }

    fn finish(self) {
        self.group.finish();
    }
}

fn query(c: &mut Criterion) {
    let rows: Vec<Row> = (0..RECORDS).map(Row::new).collect();
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), &config()).unwrap();
    seed(&db, &rows);

    let expected = |keep: &dyn Fn(&Row) -> bool| rows.iter().filter(|row| keep(row)).count() as u64;
    // The value above which the top `percent` of the numbers lie.
    let after = |percent: u64| NUMBERS - NUMBERS * percent / 100 - 1;
    let above = |percent| Filter::Range {
        field: CellNameRef::raw(b"number").into(),
        bounds: Bounds::new(
            marker::U64,
            Bound::Excluded(after(percent)),
            Bound::Unbounded,
        ),
    };
    let top = |percent| expected(&|row| row.number > after(percent));

    let mut group = Group::new(c, &db, "query/range");
    for percent in [1, 10] {
        // `number > X` over the top 1% or 10% of the values.
        group.query(&format!("{percent}%"), above(percent), None, top(percent));
    }
    group.query("limit_10", above(10), Some(10), top(10));
    group.count("count", above(10), top(10));
    // `skewed = 'small-1' AND number > X`: a 1.6% value within the 10% range.
    group.query(
        "and_small",
        Filter::And(vec![eq("skewed", "small-1"), above(10)]),
        None,
        expected(&|row| row.skewed == 1 && row.number > after(10)),
    );
    group.finish();

    let q0 = expected(&|row| row.quarter == 0);
    let mut group = Group::new(c, &db, "query/eq");
    // `quarter = 'q0'`: one term holding 25% of the records.
    group.query("quarter", eq("quarter", "q0"), None, q0);
    group.query("quarter_limit_10", eq("quarter", "q0"), Some(10), q0);
    group.count("quarter_count", eq("quarter", "q0"), q0);
    // `skewed = 'big' AND quarter = 'q0'`: two large terms, 20% and 25%.
    group.query(
        "and_big_quarter",
        Filter::And(vec![eq("skewed", "big"), eq("quarter", "q0")]),
        None,
        expected(&|row| row.skewed == 0 && row.quarter == 0),
    );
    group.finish();
}

criterion_group!(benches, query);
criterion_main!(benches);
