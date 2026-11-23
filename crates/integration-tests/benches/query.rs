//! Times queries on a freshly loaded MDBX database with 100,000 records.
//!
//! Cases spread over what drives query cost: terms per match and bitmap
//! containers per match. All run warm, and IDs span only two containers.

mod common;

use std::cell::OnceCell;
use std::hint::black_box;
use std::ops::Bound;

use criterion::measurement::WallTime;
use criterion::{BatchSize, BenchmarkGroup, Criterion, criterion_group, criterion_main};
use golemdb_api::{
    Api, Bounds, CellName, CellNameRef, CellValue, CountOp, Database, EqValue, Filter, InValues,
    Metered, QueryOp, ReadTarget, RecordOp, SortKey, marker, op,
};
use tempfile::TempDir;

use common::{BUDGET, bench_dir, config, key};

const RECORDS: u64 = 100_000;
const PER_COMMIT: u64 = 1_000;
const _: () = assert!(RECORDS.is_multiple_of(PER_COMMIT));
/// `number` values, each held by about five records.
const NUMBERS: u64 = RECORDS / 5;
/// Consecutive records sharing a `cluster` value.
const CLUSTER: u64 = 100;
const CLUSTERS: u64 = RECORDS / CLUSTER;
/// 80% into a quarter, which holds 25% of the records.
const DEEP_OFFSET: u64 = RECORDS / 5;

/// Fields drop in order, so the database closes before its directory goes.
struct Fixture {
    db: Database,
    _dir: TempDir,
}

impl Fixture {
    fn load() -> Self {
        let dir = bench_dir();
        let db = Database::open(dir.path(), &config()).unwrap();
        for first in (0..RECORDS).step_by(PER_COMMIT as usize) {
            let branch = db.begin().unwrap();
            for n in first..first + PER_COMMIT {
                db.create(branch, create(n)).into_result().unwrap();
            }
            db.commit(branch).unwrap();
        }
        Self { db, _dir: dir }
    }
}

/// Record `n`. Records per attribute value: `quarter` 25%, `skewed` ~20% for
/// `big` and ~1.6% for each `small-*`, `number` ~5 scattered, `cluster`
/// [`CLUSTER`] consecutive, `unique` one.
fn create(n: u64) -> RecordOp<op::Create> {
    let r = mix(n);
    let quarter = format!("q{}", (r >> 16) % 4);
    let skewed = match r % 100 {
        0..20 => "big".to_string(),
        _ => format!("small-{}", 1 + (r >> 8) % 49),
    };
    RecordOp::create(key(n), BUDGET)
        .attribute("quarter", CellValue::from_str(&quarter))
        .attribute("skewed", CellValue::from_str(&skewed))
        .attribute("number", CellValue::from_u64(mix(r) % NUMBERS))
        .attribute("cluster", CellValue::from_u64(n / CLUSTER))
        .attribute("unique", CellValue::from_u64(n))
}

/// SplitMix64.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

struct Group<'a> {
    fixture: &'a OnceCell<Fixture>,
    group: BenchmarkGroup<'a, WallTime>,
}

impl<'a> Group<'a> {
    fn new(c: &'a mut Criterion, fixture: &'a OnceCell<Fixture>, name: &str) -> Self {
        let group = c.benchmark_group(format!("query/{}k/{name}", RECORDS / 1000));
        Self { fixture, group }
    }

    fn query(&mut self, name: &str, filter: Filter) {
        self.query_with(name, filter, |op| op);
    }

    fn query_with(&mut self, name: &str, filter: Filter, page: impl FnOnce(QueryOp) -> QueryOp) {
        let op = page(QueryOp::from_filter(filter.clone(), BUDGET));
        self.bench(name, filter, op, |db, op| db.query(None, op));
    }

    fn count(&mut self, name: &str, filter: Filter) {
        let op = CountOp::filter(filter.clone(), BUDGET);
        self.bench(name, filter, op, |db, op| db.count(None, op));
    }

    /// Times `run` on clones of `op`, then prints how many records `filter`
    /// matches, since a page holds at most 100.
    fn bench<Op: Clone, R>(
        &mut self,
        name: &str,
        filter: Filter,
        op: Op,
        run: impl Fn(&Database, Op) -> Metered<R>,
    ) {
        let fixture = self.fixture;
        let mut matches = None;
        self.group.bench_function(name, |b| {
            let db = &fixture.get_or_init(Fixture::load).db;
            matches.get_or_insert_with(|| {
                let count = CountOp::filter(filter.clone(), BUDGET);
                db.count(None, count).into_result().unwrap()
            });
            b.iter_batched(
                || op.clone(),
                |op| run(db, black_box(op)).into_result().unwrap(),
                BatchSize::SmallInput,
            )
        });
        if let Some(matches) = matches {
            let percent = 100.0 * matches as f64 / RECORDS as f64;
            println!("{name}: {matches} matches ({percent:.2}% of records)\n");
        }
    }

    /// Gets a different key each call, from record `first` on: existing records
    /// below [`RECORDS`], missing ones from it.
    fn get(&mut self, name: &str, first: u64) {
        const STRIDE: u64 = 7_919;
        let (fixture, mut n) = (self.fixture, 0);
        let get = |db: &Database, n| db.get(ReadTarget::Head, RecordOp::get(key(n), BUDGET));
        self.group.bench_function(name, |b| {
            let db = &fixture.get_or_init(Fixture::load).db;
            let found = get(db, first).into_result().is_ok();
            assert_eq!(found, first < RECORDS, "{name}");
            b.iter_batched(
                || {
                    n = (n + STRIDE) % RECORDS;
                    first + n
                },
                |n| get(db, black_box(n)).into_result().is_ok(),
                BatchSize::SmallInput,
            )
        });
    }

    fn finish(self) {
        self.group.finish();
    }
}

fn field(name: &str) -> CellName {
    CellNameRef::raw(name.as_bytes()).into()
}

fn is(name: &str, value: &str) -> Filter {
    Filter::Eq {
        field: field(name),
        value: EqValue::of(marker::Str, value),
    }
}

fn is_u64(name: &str, value: u64) -> Filter {
    Filter::Eq {
        field: field(name),
        value: EqValue::of(marker::U64, value),
    }
}

/// `name > X` over the top `percent` of `0..values`.
fn top(name: &str, values: u64, percent: u64) -> Filter {
    let after = values - values * percent / 100 - 1;
    Filter::Range {
        field: field(name),
        bounds: Bounds::new(marker::U64, Bound::Excluded(after), Bound::Unbounded),
    }
}

/// `terms` values spread evenly across `0..values`.
fn spread(terms: u64, values: u64) -> impl Iterator<Item = u64> {
    (0..terms).map(move |i| i * values / terms)
}

fn exists(name: &str) -> Filter {
    Filter::Exists {
        field: field(name),
        ty: None,
    }
}

fn query(c: &mut Criterion) {
    // Loaded by the first case that runs.
    let fixture = OnceCell::new();
    let q0 = || is("quarter", "q0");
    let number = |percent| top("number", NUMBERS, percent);
    let unique = |percent| top("unique", RECORDS, percent);

    let mut range = Group::new(c, &fixture, "range");
    // number > X, the top 1%
    range.query("number_1%", number(1));
    // number > X, the top 10%
    range.query("number_10%", number(10));
    // number > X, the top 99%
    range.query("number_99%", number(99));
    // unique > X, the top 10%
    range.query("unique_10%", unique(10));
    // unique > X, the top 99%: a broad range with values in ID order
    range.query("unique_99%", unique(99));
    range.finish();

    let mut eq = Group::new(c, &fixture, "eq");
    // unique = X: one record
    eq.query("unique", is_u64("unique", RECORDS / 2));
    // quarter = 'q0'
    eq.query("quarter", q0());
    // quarter = 'q0' OFFSET DEEP_OFFSET
    eq.query_with("quarter_offset_deep", q0(), |op| op.offset(DEEP_OFFSET));
    // COUNT(quarter = 'q0')
    eq.count("quarter_count", q0());
    // skewed = 'big' AND quarter = 'q0'
    let big_q0 = Filter::And(vec![is("skewed", "big"), q0()]);
    eq.query("and_big_quarter", big_q0);
    eq.finish();

    let mut and = Group::new(c, &fixture, "and");
    // quarter = 'q9' AND unique > X, the top 99%: stops after the empty child
    let early_stop = Filter::And(vec![is("quarter", "q9"), unique(99)]);
    and.query("early_stop", early_stop);
    and.finish();

    let mut union = Group::new(c, &fixture, "union");
    let number_in = |terms| Filter::In {
        field: field("number"),
        values: InValues::of(marker::U64, spread(terms, NUMBERS)),
    };
    // number IN (...), 8 values
    union.query("scattered/8", number_in(8));
    // number IN (...), 512 values
    union.query("scattered/512", number_in(512));
    // cluster IN (...), 512 values
    let cluster_in = Filter::In {
        field: field("cluster"),
        values: InValues::of(marker::U64, spread(512, CLUSTERS)),
    };
    union.query("clustered/512", cluster_in);
    // number = a OR number = b OR ...: scattered/512's values, not in order
    let mut values: Vec<u64> = spread(512, NUMBERS).collect();
    values.sort_by_key(|&value| mix(value));
    let or_eqs = values.into_iter().map(|value| is_u64("number", value));
    union.query("or_eqs/512", Filter::Or(or_eqs.collect()));
    // (quarter = 'q0' OR quarter = 'q1') AND skewed = 'small-1'
    let either = Filter::Or(vec![q0(), is("quarter", "q1")]);
    union.query("nested", Filter::And(vec![either, is("skewed", "small-1")]));
    union.finish();

    let by_number = |op: QueryOp| op.sort([SortKey::asc("number", marker::U64)]);
    let mut sort = Group::new(c, &fixture, "sort");
    // quarter = 'q0' ORDER BY number
    sort.query_with("quarter", q0(), by_number);
    // quarter = 'q0' ORDER BY number OFFSET DEEP_OFFSET
    sort.query_with("quarter_offset_deep", q0(), |op| {
        by_number(op).offset(DEEP_OFFSET)
    });
    // quarter exists ORDER BY number: every record
    sort.query_with("all", exists("quarter"), by_number);
    // quarter exists ORDER BY quarter, number: ~25k ties on the page's quarter
    sort.query_with("two_keys", exists("quarter"), |op| {
        op.sort([
            SortKey::asc("quarter", marker::Str),
            SortKey::asc("number", marker::U64),
        ])
    });
    // quarter = 'q0' ORDER BY unique DESC LIMIT 10: the latest records
    sort.query_with("quarter_latest_10", q0(), |op| {
        op.sort([SortKey::desc("unique", marker::U64)]).limit(10)
    });
    sort.finish();

    let mut scan = Group::new(c, &fixture, "scan");
    // skewed starts with 'small-1'
    let prefix = Filter::Prefix {
        field: field("skewed"),
        prefix: "small-1".into(),
    };
    scan.query("prefix", prefix);
    // quarter exists, of any type
    scan.query("exists", exists("quarter"));
    scan.finish();

    let mut get = Group::new(c, &fixture, "get");
    // get by key
    get.get("key", 0);
    // get by a key no record has
    get.get("missing", RECORDS);
    get.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().noise_threshold(0.03);
    targets = query
}
criterion_main!(benches);
