//! Times queries on a freshly loaded MDBX database with 100,000 records.
//!
//! Cases spread over what drives query cost: terms per match and bitmap
//! containers per match. All run warm, and IDs span only two containers.

mod common;

use std::hint::black_box;
use std::ops::Bound;

use criterion::measurement::WallTime;
use criterion::{BatchSize, BenchmarkGroup, Criterion, criterion_group, criterion_main};
use golemdb_api::{
    Api, Bounds, CellName, CellNameRef, CellValue, CountOp, Database, EqValue, ExistsType, Filter,
    InValues, Metered, QueryOp, ReadTarget, RecordOp, SortKey, marker, op,
};

use common::{BUDGET, bench_dir, config, key};

const RECORDS: u64 = 100_000;
const QUARTERS: u64 = 4;
const NUMBERS: u64 = RECORDS / 5;
/// Consecutive records sharing a `block`.
const BLOCK: u64 = 100;
const BLOCKS: u64 = RECORDS / BLOCK;
/// 80% into a quarter's matches.
const DEEP_OFFSET: u64 = RECORDS / QUARTERS * 4 / 5;
const PER_COMMIT: u64 = 1_000;

fn seed(db: &Database) {
    for first in (0..RECORDS).step_by(PER_COMMIT as usize) {
        let branch = db.begin().unwrap();
        for n in first..(first + PER_COMMIT).min(RECORDS) {
            db.create(branch, create(n)).into_result().unwrap();
        }
        db.commit(branch).unwrap();
    }
}

/// Record `n`: `quarter` 25% each, `skewed` `big` ~20% and `small-*` ~1.6%
/// each, `number` ~5 scattered records each, `block` [`BLOCK`] consecutive
/// records each, `unique` one each.
fn create(n: u64) -> RecordOp<op::Create> {
    let r = mix(n);
    let quarter = format!("q{}", (r >> 16) % QUARTERS);
    let skewed = match r % 100 {
        0..20 => "big".to_string(),
        _ => format!("small-{}", 1 + (r >> 8) % 49),
    };
    RecordOp::create(key(n), BUDGET)
        .attribute("quarter", CellValue::from_str(&quarter))
        .attribute("skewed", CellValue::from_str(&skewed))
        .attribute("number", CellValue::from_u64(mix(r) % NUMBERS))
        .attribute("block", CellValue::from_u64(n / BLOCK))
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
    db: &'a Database,
    group: BenchmarkGroup<'a, WallTime>,
}

impl<'a> Group<'a> {
    fn new(c: &'a mut Criterion, db: &'a Database, name: &str) -> Self {
        let group = c.benchmark_group(format!("query/{}k/{name}", RECORDS / 1000));
        Self { db, group }
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
        let db = self.db;
        let mut matches = None;
        self.group.bench_function(name, |b| {
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

    fn get(&mut self, name: &str) {
        const STRIDE: u64 = 7_919;
        let (db, mut n) = (self.db, 0);
        self.group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    n = (n + STRIDE) % RECORDS;
                    RecordOp::get(key(n), BUDGET)
                },
                |op| {
                    db.get(ReadTarget::Head, black_box(op))
                        .into_result()
                        .unwrap()
                },
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

/// `name > X` over the top `percent` of `0..values`.
fn top(name: &str, values: u64, percent: u64) -> Filter {
    let after = values - values * percent / 100 - 1;
    Filter::Range {
        field: field(name),
        bounds: Bounds::new(marker::U64, Bound::Excluded(after), Bound::Unbounded),
    }
}

/// `name IN (...)` over `terms` values spread evenly across `0..values`.
fn spread(name: &str, terms: u64, values: u64) -> Filter {
    Filter::In {
        field: field(name),
        values: InValues::of(marker::U64, (0..terms).map(|i| i * values / terms)),
    }
}

fn query(c: &mut Criterion) {
    let dir = bench_dir();
    let db = Database::open(dir.path(), &config()).unwrap();
    seed(&db);
    let q0 = || is("quarter", "q0");
    let number = |percent| top("number", NUMBERS, percent);

    let mut range = Group::new(c, &db, "range");
    range.query("1%", number(1));
    range.query("10%", number(10));
    range.query("unique_1%", top("unique", RECORDS, 1));
    range.query("unique_10%", top("unique", RECORDS, 10));
    range.query_with("limit_10", number(10), |op| op.limit(10));
    range.count("count", number(10));
    range.query(
        "and_small",
        Filter::And(vec![is("skewed", "small-1"), number(10)]),
    );
    range.finish();

    let mut eq = Group::new(c, &db, "eq");
    eq.query("quarter", q0());
    eq.query_with("quarter_limit_10", q0(), |op| op.limit(10));
    eq.query_with("quarter_offset_deep", q0(), |op| op.offset(DEEP_OFFSET));
    eq.count("quarter_count", q0());
    let big_q0 = Filter::And(vec![is("skewed", "big"), q0()]);
    eq.query("and_big_quarter", big_q0);
    eq.finish();

    let mut union = Group::new(c, &db, "union");
    for terms in [8, 64, 512] {
        union.query(
            &format!("scattered/{terms}"),
            spread("number", terms, NUMBERS),
        );
        union.query(
            &format!("clustered/{terms}"),
            spread("block", terms, BLOCKS),
        );
    }
    let overlapping = vec![q0(), is("quarter", "q1"), is("skewed", "big")];
    union.query("or_overlapping", Filter::Or(overlapping));
    union.finish();

    let by_number = |op: QueryOp| op.sort([SortKey::asc("number", marker::U64)]);
    let mut sort = Group::new(c, &db, "sort");
    sort.query_with("quarter", q0(), by_number);
    sort.query_with("quarter_offset_deep", q0(), |op| {
        by_number(op).offset(DEEP_OFFSET)
    });
    sort.finish();

    let mut scan = Group::new(c, &db, "scan");
    let prefix = Filter::Prefix {
        field: field("skewed"),
        prefix: "small-1".into(),
    };
    scan.query("prefix", prefix);
    let exists = Filter::Exists {
        field: field("number"),
        ty: Some(ExistsType::of(marker::U64)),
    };
    scan.query("exists", exists);
    scan.finish();

    let mut get = Group::new(c, &db, "get");
    get.get("key");
    get.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().noise_threshold(0.03);
    targets = query
}
criterion_main!(benches);
